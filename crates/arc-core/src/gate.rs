//! The execution gate. Every tool call — from the fixed-command router, the
//! language model, or an automation — goes through [`Gate::run`]:
//!
//! 1. unknown tools are refused;
//! 2. the tool assesses the concrete call (`Tool::assess`); blocked calls are
//!    refused outright and can never be confirmed;
//! 3. the permission [`Policy`] decides allow / confirm / deny;
//! 4. `Confirm` stores the exact call behind a single-use, expiring id and
//!    returns it — nothing runs until [`Gate::confirm`] is called with that id.

use arc_config::Config;
use arc_proto::{ActionOutcome, ActionRecord, PendingConfirmation, RiskLevel};
use arc_security::confirm::{ConfirmError, Confirmations};
use arc_security::{Decision, Policy};
use arc_tools::{JsonMap, ToolResult, Tools};
use serde_json::Value as Json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum Outcome {
    /// The tool ran.
    Done { tool: String, result: ToolResult, args: Json, risk: RiskLevel, duration_ms: u64 },
    /// Held for the user's confirmation. Nothing has run.
    NeedsConfirmation(PendingConfirmation),
    /// Refused; nothing ran.
    Denied { tool: String, reason: String, args: Json, risk: RiskLevel },
}

fn to_json(args: &JsonMap) -> Json {
    Json::Object(args.clone().into_iter().collect())
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

impl Outcome {
    pub fn tool(&self) -> &str {
        match self {
            Outcome::Done { tool, .. } | Outcome::Denied { tool, .. } => tool,
            Outcome::NeedsConfirmation(p) => &p.tool,
        }
    }

    /// Protocol record of this outcome (for replies, events, audit).
    pub fn record(&self) -> ActionRecord {
        match self {
            Outcome::Done { tool, result, args, risk, duration_ms } => {
                let (outcome, summary, data) = match result {
                    ToolResult::Ok(v) => (ActionOutcome::Success, short(&v.to_string(), 200), v.clone()),
                    ToolResult::Error(e) => (ActionOutcome::Failed, short(e, 200), Json::Null),
                };
                ActionRecord {
                    tool: tool.clone(),
                    args: args.clone(),
                    risk: *risk,
                    outcome,
                    summary,
                    data,
                    duration_ms: *duration_ms,
                }
            }
            Outcome::NeedsConfirmation(p) => ActionRecord {
                tool: p.tool.clone(),
                args: p.args.clone(),
                risk: p.risk,
                outcome: ActionOutcome::AwaitingConfirmation,
                summary: p.explanation.clone(),
                data: Json::Null,
                duration_ms: 0,
            },
            Outcome::Denied { tool, reason, args, risk } => ActionRecord {
                tool: tool.clone(),
                args: args.clone(),
                risk: *risk,
                outcome: ActionOutcome::Denied,
                summary: reason.clone(),
                data: Json::Null,
                duration_ms: 0,
            },
        }
    }
}

pub struct Gate {
    tools: Arc<Tools>,
    policy: Policy,
    pending: Mutex<Confirmations>,
}

impl Gate {
    pub fn new(tools: Arc<Tools>, cfg: &Config) -> Self {
        Self {
            tools,
            policy: Policy::new(&cfg.permissions, &cfg.tools.disabled),
            pending: Mutex::new(Confirmations::new(Duration::from_secs(
                cfg.permissions.confirmation_timeout_s.max(1),
            ))),
        }
    }

    pub fn tools(&self) -> &Tools {
        &self.tools
    }

    pub fn is_disabled(&self, tool: &str) -> bool {
        self.policy.is_disabled(tool)
    }

    async fn execute(t: &Arc<dyn arc_tools::Tool>, tool: &str, args: &JsonMap, risk: RiskLevel) -> Outcome {
        let start = Instant::now();
        let result = t.execute(args).await;
        Outcome::Done {
            tool: tool.into(),
            result,
            args: to_json(args),
            risk,
            duration_ms: start.elapsed().as_millis() as u64,
        }
    }

    /// Run a tool call subject to policy. `context` is opaque data returned
    /// with the confirmation (e.g. which conversation to resume).
    pub async fn run(&self, tool: &str, args: JsonMap, context: Json) -> Outcome {
        let Some(t) = self.tools.by_name(tool).cloned() else {
            return Outcome::Denied {
                tool: tool.into(),
                reason: format!("unknown tool `{tool}`"),
                args: to_json(&args),
                risk: RiskLevel::Safe,
            };
        };
        let a = t.assess(&args);
        if let Some(reason) = a.blocked {
            return Outcome::Denied { tool: tool.into(), reason, args: to_json(&args), risk: a.risk };
        }
        let decision = match self.policy.decide(tool, a.risk) {
            Decision::Allow if a.force_confirm => Decision::Confirm { reason: "unlisted command".into() },
            d => d,
        };
        match decision {
            Decision::Deny { reason } => {
                Outcome::Denied { tool: tool.into(), reason, args: to_json(&args), risk: a.risk }
            }
            Decision::Confirm { .. } => {
                let p = self.pending.lock().unwrap().issue(
                    tool,
                    to_json(&args),
                    a.risk,
                    &a.explanation,
                    context,
                    Instant::now(),
                );
                tracing::info!(tool, risk = %a.risk, id = %p.confirmation_id, "held for confirmation");
                Outcome::NeedsConfirmation(p)
            }
            Decision::Allow => Self::execute(&t, tool, &args, a.risk).await,
        }
    }

    /// Execute a previously held call, verbatim. Single use.
    pub async fn confirm(&self, id: &str) -> Result<Outcome, ConfirmError> {
        let p = self.pending.lock().unwrap().take(id, Instant::now())?;
        let args: JsonMap = match &p.args {
            Json::Object(m) => m.clone().into_iter().collect(),
            _ => JsonMap::new(),
        };
        let Some(t) = self.tools.by_name(&p.tool).cloned() else {
            return Ok(Outcome::Denied {
                tool: p.tool,
                reason: "tool no longer available".into(),
                args: p.args,
                risk: p.risk,
            });
        };
        // Re-check: a confirmation can never unlock a blocked call.
        if let Some(reason) = t.assess(&args).blocked {
            return Ok(Outcome::Denied { tool: p.tool, reason, args: p.args, risk: p.risk });
        }
        tracing::info!(tool = %p.tool, id, "confirmed; executing");
        Ok(Self::execute(&t, &p.tool, &args, p.risk).await)
    }

    pub fn cancel(&self, id: &str) -> bool {
        self.pending.lock().unwrap().cancel(id).is_some()
    }

    pub fn cancel_all(&self) -> usize {
        self.pending.lock().unwrap().cancel_all()
    }

    /// Id of the most recent live confirmation (for a bare "yes"/"no").
    pub fn latest_pending(&self) -> Option<String> {
        self.latest_pending_info().map(|(id, _)| id)
    }

    /// Id and risk of the most recent live confirmation.
    pub fn latest_pending_info(&self) -> Option<(String, RiskLevel)> {
        self.pending.lock().unwrap().latest(Instant::now()).map(|p| (p.id.clone(), p.risk))
    }

    /// Effective risk of a call without running it (used by UIs).
    pub fn risk_of(&self, tool: &str, args: &JsonMap) -> Option<RiskLevel> {
        self.tools.by_name(tool).map(|t| t.assess(args).risk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_tools::Tool;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Probe {
        name: &'static str,
        risk: RiskLevel,
        runs: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for Probe {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "test probe"
        }
        fn base_risk(&self) -> RiskLevel {
            self.risk
        }
        async fn execute(&self, _: &JsonMap) -> ToolResult {
            self.runs.fetch_add(1, Ordering::SeqCst);
            ToolResult::Ok(Json::Null)
        }
    }

    fn gate(cfg: &Config) -> (Gate, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let safe = Arc::new(AtomicUsize::new(0));
        let danger = Arc::new(AtomicUsize::new(0));
        let mut tools = Tools::new();
        tools.register(Arc::new(Probe { name: "probe.safe", risk: RiskLevel::Safe, runs: safe.clone() }));
        tools.register(Arc::new(Probe {
            name: "probe.danger",
            risk: RiskLevel::Dangerous,
            runs: danger.clone(),
        }));
        (Gate::new(Arc::new(tools), cfg), safe, danger)
    }

    #[tokio::test]
    async fn safe_tool_runs_immediately() {
        let (g, safe, _) = gate(&Config::default());
        assert!(matches!(g.run("probe.safe", JsonMap::new(), Json::Null).await, Outcome::Done { .. }));
        assert_eq!(safe.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dangerous_tool_waits_for_confirmation_and_runs_once() {
        let (g, _, danger) = gate(&Config::default());
        let Outcome::NeedsConfirmation(p) = g.run("probe.danger", JsonMap::new(), Json::Null).await else {
            panic!("expected confirmation")
        };
        assert_eq!(danger.load(Ordering::SeqCst), 0, "must not run before confirmation");
        assert!(matches!(g.confirm(&p.confirmation_id).await, Ok(Outcome::Done { .. })));
        assert_eq!(danger.load(Ordering::SeqCst), 1);
        assert_eq!(g.confirm(&p.confirmation_id).await.unwrap_err(), ConfirmError::Unknown);
        assert_eq!(danger.load(Ordering::SeqCst), 1, "confirmation is single use");
    }

    #[tokio::test]
    async fn cancel_discards_pending() {
        let (g, _, danger) = gate(&Config::default());
        let Outcome::NeedsConfirmation(p) = g.run("probe.danger", JsonMap::new(), Json::Null).await else {
            panic!()
        };
        assert_eq!(g.latest_pending().as_deref(), Some(p.confirmation_id.as_str()));
        assert!(g.cancel(&p.confirmation_id));
        assert!(g.confirm(&p.confirmation_id).await.is_err());
        assert_eq!(danger.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn real_power_tools_are_held() {
        let (g, _, _) = gate(&Config::default());
        for t in ["reboot", "shutdown"] {
            assert!(
                matches!(g.run(t, JsonMap::new(), Json::Null).await, Outcome::NeedsConfirmation(_)),
                "{t}"
            );
        }
    }

    #[tokio::test]
    async fn blocked_shell_is_denied_not_held() {
        let (g, _, _) = gate(&Config::default());
        let mut a = JsonMap::new();
        a.insert("command".into(), Json::String("rm -rf /".into()));
        assert!(matches!(g.run("shell_exec", a, Json::Null).await, Outcome::Denied { .. }));
    }

    #[tokio::test]
    async fn disabled_and_unknown_tools_are_denied() {
        let mut cfg = Config::default();
        cfg.tools.disabled = vec!["probe.safe".into()];
        let (g, safe, _) = gate(&cfg);
        assert!(matches!(g.run("probe.safe", JsonMap::new(), Json::Null).await, Outcome::Denied { .. }));
        assert!(matches!(g.run("nope", JsonMap::new(), Json::Null).await, Outcome::Denied { .. }));
        assert_eq!(safe.load(Ordering::SeqCst), 0);
    }
}
