//! The execution gate. Every tool call — from the fixed-command router, the
//! language model, or an automation — goes through [`Gate::run`]:
//!
//! 1. unknown tools are refused;
//! 2. the tool assesses the concrete call (`Tool::assess`); blocked calls are
//!    refused outright and can never be confirmed;
//! 3. the user's classification for the tool (set in the Arc app) replaces the
//!    tool's own risk level, so `dangerous` always asks and `safe` never does;
//! 4. the permission [`Policy`] decides allow / confirm / deny;
//! 5. `Confirm` stores the exact call behind a single-use, expiring id and
//!    returns it — nothing runs until [`Gate::confirm`] is called with that id.
//!
//! Step 3 sits above the policy, not below it. A classification is a statement
//! about the *tool* ("never let it run unattended"), while a policy is a
//! statement about the *machine* ("no tool may be denied"), and the user's own
//! click should decide how careful Arc is. A `deny` rule still wins, and a
//! blocked call is still never runnable — see [`Gate::classify`].

use arc_config::Config;
use arc_config::classification::ClassifiedTools;
use arc_proto::{ActionOutcome, ActionRecord, PendingConfirmation, RiskLevel};
use arc_security::confirm::{ConfirmError, Confirmations};
use arc_security::{Decision, Policy};
use arc_tools::{Assessment, JsonMap, ToolResult, Tools};
use serde_json::Value as Json;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum Outcome {
    /// The tool ran.
    Done {
        tool: String,
        result: ToolResult,
        args: Json,
        risk: RiskLevel,
        duration_ms: u64,
        /// Set when the tool's classification is `caution`: the call ran
        /// without a prompt, and the UI says so.
        warning: Option<String>,
    },
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
            Outcome::Done { tool, result, args, risk, duration_ms, warning } => {
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
                    warning: warning.clone(),
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
                warning: None,
                data: Json::Null,
                duration_ms: 0,
            },
            Outcome::Denied { tool, reason, args, risk } => ActionRecord {
                tool: tool.clone(),
                args: args.clone(),
                risk: *risk,
                outcome: ActionOutcome::Denied,
                summary: reason.clone(),
                warning: None,
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
    /// The user's per-tool classifications, edited from the Arc app. Behind a
    /// lock because the daemon changes them at runtime while turns are in
    /// flight, and the next call has to see the new value.
    classes: RwLock<ClassifiedTools>,
}

impl Gate {
    pub fn new(tools: Arc<Tools>, cfg: &Config) -> Self {
        Self::with_classes(tools, cfg, ClassifiedTools::in_memory())
    }

    /// As [`Gate::new`], but with the user's saved classifications attached.
    /// The daemon loads these from disk once at start; everything else (tests,
    /// `Assistant::from_config` in a library context) uses [`Gate::new`].
    pub fn with_classes(tools: Arc<Tools>, cfg: &Config, classes: ClassifiedTools) -> Self {
        Self {
            tools,
            policy: Policy::new(&cfg.permissions, &cfg.tools.disabled),
            pending: Mutex::new(Confirmations::new(Duration::from_secs(
                cfg.permissions.confirmation_timeout_s.max(1),
            ))),
            classes: RwLock::new(classes),
        }
    }

    pub fn tools(&self) -> &Tools {
        &self.tools
    }

    pub fn is_disabled(&self, tool: &str) -> bool {
        self.policy.is_disabled(tool)
    }

    /// The classification the user has set for this tool, if any.
    pub fn classification(&self, tool: &str) -> Option<RiskLevel> {
        self.classes.read().unwrap().get(tool)
    }

    /// Set (or with `None`, clear) a tool's classification and persist it.
    /// Returns the tool's built-in level alongside, so the UI can show what it
    /// is overriding.
    pub fn set_classification(
        &self,
        tool: &str,
        level: Option<RiskLevel>,
    ) -> Result<(RiskLevel, bool), String> {
        let Some(t) = self.tools.by_name(tool) else {
            return Err(format!("unknown tool `{tool}`"));
        };
        // A tool that is dangerous by nature (reboot, shutdown, the code
        // agent) can be made stricter but not looser. Lowering it would only
        // look like it worked: classify() keeps any call the tool itself rates
        // dangerous at dangerous, so the picker would show "safe" on a tool
        // that still prompts. Refuse, and say why.
        if let Some(l) = level {
            if t.base_risk() == RiskLevel::Dangerous && l < RiskLevel::Dangerous && !t.user_may_lower() {
                return Err(format!(
                    "`{tool}` is dangerous by nature and cannot be lowered; it can only be reset to default"
                ));
            }
        }
        // Choosing the level the tool already ships with is a reset, not an
        // override: storing it would mark the row "reclassified" and pin the
        // tool to today's level if a later version of Arc changes its default.
        let level = level.filter(|&l| l != t.base_risk());
        // Persist first: a classification the daemon could not write is one
        // that would silently vanish on restart, and a setting that does not
        // survive a restart is worse than an error.
        self.classes.write().unwrap().set(tool, level)?;
        let base = self.tools.by_name(tool).map(|t| t.base_risk()).unwrap_or(RiskLevel::Safe);
        Ok((base, self.classification(tool).is_some()))
    }

    /// The risk the runtime will actually apply to a call, after the user's
    /// classification.
    ///
    /// A classification replaces the tool's *baseline*, never its judgement of
    /// a particular call.
    ///
    /// Raising is unconditional: `dangerous` forces a confirmation even where
    /// a policy would have allowed the call. Lowering only moves ordinary
    /// calls. If the tool rates *this* call dangerous (`shell_exec` looking at
    /// `rm -rf` or `systemctl poweroff`), it stays dangerous and still asks;
    /// and a confirmation the tool or config demands (`confirm_unlisted`,
    /// `code.confirm`) is kept. The first version replaced the per-call risk
    /// outright, so marking `shell_exec` "caution" let `systemctl poweroff`
    /// run unasked -- found by actually powering the machine off.
    ///
    /// Blocked calls and `deny` rules are handled before and after this and
    /// are never affected by it.
    ///
    /// The exception is a tool whose danger is only "nobody has reviewed
    /// this yet" (`Tool::user_may_lower`: Arc's self-made scripts). It has no
    /// per-call judgement to preserve -- every call is the same script -- so
    /// once the user has read it and lowered it, the lowered level is the
    /// level. Its content was screened against the blocklist at creation and
    /// is again at every load, and a changed script loses the classification.
    fn classify(&self, t: &dyn arc_tools::Tool, tool: &str, a: &Assessment) -> (RiskLevel, bool) {
        match self.classification(tool) {
            None => (a.risk, a.force_confirm),
            Some(RiskLevel::Dangerous) => (RiskLevel::Dangerous, true),
            Some(l) if t.user_may_lower() => (l, false),
            Some(_) if a.risk == RiskLevel::Dangerous => (RiskLevel::Dangerous, a.force_confirm),
            Some(l) => (l, a.force_confirm),
        }
    }

    /// True when this call ran under a `caution` classification, which the
    /// UI reports as a warning.
    fn caution_notice(&self, tool: &str) -> Option<String> {
        (self.classification(tool) == Some(RiskLevel::Caution))
            .then(|| format!("{tool} is classified as caution, so it ran without asking"))
    }

    async fn execute(
        t: &Arc<dyn arc_tools::Tool>,
        tool: &str,
        args: &JsonMap,
        risk: RiskLevel,
        warning: Option<String>,
    ) -> Outcome {
        let start = Instant::now();
        let result = t.execute(args).await;
        Outcome::Done {
            tool: tool.into(),
            result,
            args: to_json(args),
            risk,
            duration_ms: start.elapsed().as_millis() as u64,
            warning,
        }
    }

    /// Run a tool call subject to policy. `context` is opaque data returned
    /// with the confirmation (e.g. which conversation to resume).
    pub async fn run(&self, tool: &str, args: JsonMap, context: Json) -> Outcome {
        if self.tools.custom().composite_steps(tool).is_some() {
            return self.run_composite(tool, args, context, false).await;
        }
        let Some(t) = self.tools.by_name(tool) else {
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
        // The user's classification replaces the tool's own level. Blocked is
        // already handled above, so a `safe` classification cannot unblock a
        // destructive call -- it only removes the prompt.
        let (risk, force_confirm) = self.classify(t.as_ref(), tool, &a);
        let decision = match self.policy.decide(tool, risk) {
            Decision::Allow if force_confirm => Decision::Confirm { reason: "unlisted command".into() },
            d => d,
        };
        match decision {
            Decision::Deny { reason } => {
                Outcome::Denied { tool: tool.into(), reason, args: to_json(&args), risk }
            }
            Decision::Confirm { .. } => {
                let p = self.pending.lock().unwrap().issue(
                    tool,
                    to_json(&args),
                    risk,
                    &a.explanation,
                    context,
                    Instant::now(),
                );
                tracing::info!(tool, risk = %risk, id = %p.confirmation_id, "held for confirmation");
                Outcome::NeedsConfirmation(p)
            }
            Decision::Allow => {
                let warning = self.caution_notice(tool);
                let out = Self::execute(&t, tool, &args, risk, warning).await;
                self.after(&out);
                out
            }
        }
    }

    /// A deleted self-made tool takes its classification with it. Otherwise
    /// "delete screenshot, create screenshot" would hand a brand-new,
    /// unreviewed script the `safe` the user gave the old one.
    fn after(&self, out: &Outcome) {
        if let Outcome::Done { tool, result: ToolResult::Ok(v), .. } = out {
            if tool == "tool_delete" {
                if let Some(name) = v.get("deleted").and_then(|n| n.as_str()) {
                    if let Err(e) = self.classes.write().unwrap().set(name, None) {
                        tracing::warn!(tool = name, error = %e, "could not clear a deleted tool's classification");
                    }
                }
            }
        }
    }

    /// Clear the classification of every named tool (self-made scripts whose
    /// content changed on disk since they were created). Returns those that
    /// had one.
    pub fn forget_classifications(&self, tools: &[String]) -> Vec<String> {
        let mut cleared = vec![];
        for t in tools {
            if self.classification(t).is_some() && self.classes.write().unwrap().set(t, None).is_ok() {
                cleared.push(t.clone());
            }
        }
        cleared
    }

    /// Execute a previously held call, verbatim. Single use.
    pub async fn confirm(&self, id: &str) -> Result<Outcome, ConfirmError> {
        let p = self.pending.lock().unwrap().take(id, Instant::now())?;
        let args: JsonMap = match &p.args {
            Json::Object(m) => m.clone().into_iter().collect(),
            _ => JsonMap::new(),
        };
        // A composite held as a whole (the user raised it to dangerous) runs
        // its steps now -- each still individually gated.
        if self.tools.custom().composite_steps(&p.tool).is_some() {
            tracing::info!(tool = %p.tool, id, "confirmed; running composite");
            return Ok(self.run_composite(&p.tool, args, p.context, true).await);
        }
        let Some(t) = self.tools.by_name(&p.tool) else {
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
        // Re-apply the classification: the user may have downgraded the tool
        // between asking and clicking, and the stored call is what runs.
        let warning = self.caution_notice(&p.tool);
        let out = Self::execute(&t, &p.tool, &args, p.risk, warning).await;
        self.after(&out);
        Ok(out)
    }

    /// Run a tool Arc made from other tools.
    ///
    /// The composite is a name, not a permission: every step goes through
    /// [`Gate::run`] exactly as if the model had called it directly, so its
    /// own assessment, classification, policy and confirmation all apply.
    /// Wrapping `shell_exec "systemctl poweroff"` in a composite called
    /// `tidy_up` changes nothing about whether it asks.
    ///
    /// On top of that, the composite's own row can be raised: marked
    /// dangerous (or denied in config), the whole chain is held or refused
    /// before any step runs. `confirmed` is set when the user already
    /// approved that hold.
    ///
    /// A step that needs confirmation stops the chain there: steps before it
    /// have run, the held step runs alone if approved, and later steps do not
    /// run -- the reply says so, rather than implying the whole thing finished.
    async fn run_composite(&self, tool: &str, args: JsonMap, context: Json, confirmed: bool) -> Outcome {
        let Some((steps, params)) = self.tools.custom().composite_steps(tool) else {
            return Outcome::Denied {
                tool: tool.into(),
                reason: format!("`{tool}` no longer exists"),
                args: to_json(&args),
                risk: RiskLevel::Safe,
            };
        };
        let risk = self.base_risk_of(tool).unwrap_or(RiskLevel::Safe);
        let missing: Vec<&String> = params.keys().filter(|k| !args.contains_key(*k)).collect();
        if !missing.is_empty() {
            let result = ToolResult::Error(format!("missing argument(s) {missing:?} for `{tool}`"));
            return Outcome::Done {
                tool: tool.into(),
                result,
                args: to_json(&args),
                risk,
                duration_ms: 0,
                warning: None,
            };
        }
        if !confirmed {
            let class = self.classification(tool);
            let decision = self.policy.decide(tool, class.unwrap_or(RiskLevel::Safe));
            if let Decision::Deny { reason } = decision {
                return Outcome::Denied { tool: tool.into(), reason, args: to_json(&args), risk };
            }
            if class == Some(RiskLevel::Dangerous) {
                let explanation = format!("run my tool `{tool}` ({} steps)", steps.len());
                let p = self.pending.lock().unwrap().issue(
                    tool,
                    to_json(&args),
                    RiskLevel::Dangerous,
                    &explanation,
                    context,
                    Instant::now(),
                );
                tracing::info!(tool, id = %p.confirmation_id, "composite held for confirmation");
                return Outcome::NeedsConfirmation(p);
            }
        }
        let start = Instant::now();
        let mut done: Vec<Json> = vec![];
        let mut warnings: Vec<String> = vec![];
        let total = steps.len();
        for (i, step) in steps.iter().enumerate() {
            let filled: JsonMap = match arc_tools::custom::fill(&step.args, &args) {
                Json::Object(m) => m.into_iter().collect(),
                _ => JsonMap::new(),
            };
            // Steps are never composites (creation refuses it), so this
            // recursion is one level deep.
            let outcome = Box::pin(self.run(&step.tool, filled, context.clone())).await;
            let n = i + 1;
            match outcome {
                Outcome::Done { result: ToolResult::Ok(v), warning, .. } => {
                    if let Some(w) = warning {
                        warnings.push(w);
                    }
                    done.push(serde_json::json!({"step": n, "tool": step.tool, "result": v}));
                }
                Outcome::Done { result: ToolResult::Error(e), .. } => {
                    let result = ToolResult::Error(format!(
                        "`{tool}` stopped at step {n} of {total} ({}): {e}. Steps before it ran.",
                        step.tool
                    ));
                    return Outcome::Done {
                        tool: tool.into(),
                        result,
                        args: to_json(&args),
                        risk,
                        duration_ms: start.elapsed().as_millis() as u64,
                        warning: None,
                    };
                }
                Outcome::Denied { reason, .. } => {
                    return Outcome::Denied {
                        tool: tool.into(),
                        reason: format!("step {n} of {total} ({}) was refused: {reason}", step.tool),
                        args: to_json(&args),
                        risk,
                    };
                }
                Outcome::NeedsConfirmation(mut p) => {
                    let rest = total - n;
                    p.explanation = if rest == 0 {
                        format!("{} (step {n} of {total} of `{tool}`)", p.explanation)
                    } else {
                        format!(
                            "{} (step {n} of {total} of `{tool}`; the {rest} step(s) after it won't run until you ask again)",
                            p.explanation
                        )
                    };
                    return Outcome::NeedsConfirmation(p);
                }
            }
        }
        // A composite reports as one action under its own name, so a `code`
        // step's real numbers stayed buried in the step list and the spoken
        // completion line fell back to counting actions: measured live, a
        // five-minute Hermes run announced as "2 steps, 303 seconds". Lift
        // them to the top level, where a listener expects to find them.
        let mut top = serde_json::Map::new();
        top.insert("steps".into(), serde_json::json!(done));
        for step in &done {
            let is_code = step["tool"] == "code";
            if !is_code {
                continue;
            }
            // Under its own key: `steps` at the top level is the composite's
            // own step list, and overwriting it would lose the breakdown.
            let mut inner = serde_json::Map::new();
            for k in ["steps", "took_s", "status", "session"] {
                if let Some(v) = step["result"].get(k) {
                    inner.insert(k.to_string(), v.clone());
                }
            }
            top.insert("code".into(), serde_json::Value::Object(inner));
            break;
        }
        Outcome::Done {
            tool: tool.into(),
            result: ToolResult::Ok(serde_json::Value::Object(top)),
            args: to_json(&args),
            risk,
            duration_ms: start.elapsed().as_millis() as u64,
            warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
        }
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

    /// Effective risk of a call without running it (used by UIs): the
    /// classification if the user set one, else what the tool assesses.
    pub fn risk_of(&self, tool: &str, args: &JsonMap) -> Option<RiskLevel> {
        self.tools.by_name(tool).map(|t| {
            let a = t.assess(args);
            // A blocked call is refused whatever the classification says, so
            // reporting its risk as anything but the tool's own would be a lie.
            if a.blocked.is_some() { a.risk } else { self.classify(t.as_ref(), tool, &a).0 }
        })
    }

    /// The risk a tool declares in code, ignoring any classification.
    pub fn base_risk_of(&self, tool: &str) -> Option<RiskLevel> {
        self.tools.by_name(tool).map(|t| t.base_risk())
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

    // -----------------------------------------------------------------------
    // User classifications (the Arc app's per-tool safety dropdown)
    // -----------------------------------------------------------------------

    /// The gate with a classifications store backed by a real file, so these
    /// tests cover persistence as well as enforcement.
    fn classified_gate() -> (Gate, Arc<AtomicUsize>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let classes = ClassifiedTools::load_from(dir.path().join("tool_classes.json"));
        let (g, safe, _) = gate_with(&Config::default(), classes);
        // probe.safe is registered as Safe; make it Dangerous and watch what
        // the gate does about it.
        g.set_classification("probe.safe", Some(RiskLevel::Dangerous)).unwrap();
        (g, safe, dir)
    }

    fn gate_with(cfg: &Config, classes: ClassifiedTools) -> (Gate, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let safe = Arc::new(AtomicUsize::new(0));
        let danger = Arc::new(AtomicUsize::new(0));
        let mut tools = Tools::new();
        tools.register(Arc::new(Probe { name: "probe.safe", risk: RiskLevel::Safe, runs: safe.clone() }));
        tools.register(Arc::new(Probe {
            name: "probe.danger",
            risk: RiskLevel::Dangerous,
            runs: danger.clone(),
        }));
        (Gate::with_classes(Arc::new(tools), cfg, classes), safe, danger)
    }

    /// The headline behaviour: a tool switched to `dangerous` in the UI must
    /// stop running unattended, even though it is Safe in code and the policy
    /// would otherwise let it straight through.
    #[tokio::test]
    async fn a_tool_switched_to_dangerous_needs_a_confirmation_click() {
        let (g, safe, _dir) = classified_gate();
        // Sanity: the code says Safe, and without a classification it runs.
        assert_eq!(g.base_risk_of("probe.safe"), Some(RiskLevel::Safe));
        assert_eq!(g.risk_of("probe.safe", &JsonMap::new()), Some(RiskLevel::Dangerous));
        assert_eq!(g.classification("probe.safe"), Some(RiskLevel::Dangerous));

        let Outcome::NeedsConfirmation(p) = g.run("probe.safe", JsonMap::new(), Json::Null).await else {
            panic!("a dangerous tool must be held for confirmation")
        };
        assert_eq!(p.risk, RiskLevel::Dangerous, "the prompt must say what is at stake");
        assert_eq!(p.tool, "probe.safe");
        assert_eq!(safe.load(Ordering::SeqCst), 0, "nothing may run before the user clicks");

        // The click is what runs it -- and it runs exactly once.
        let Ok(Outcome::Done { .. }) = g.confirm(&p.confirmation_id).await else { panic!() };
        assert_eq!(safe.load(Ordering::SeqCst), 1);
        assert_eq!(g.confirm(&p.confirmation_id).await.unwrap_err(), ConfirmError::Unknown);
        assert_eq!(safe.load(Ordering::SeqCst), 1, "the confirmation is single use");
    }

    /// Rejecting is still a rejection: the extra click is a gate, not a delay.
    #[tokio::test]
    async fn a_dangerous_tool_can_still_be_cancelled() {
        let (g, safe, _dir) = classified_gate();
        let Outcome::NeedsConfirmation(p) = g.run("probe.safe", JsonMap::new(), Json::Null).await else {
            panic!()
        };
        assert!(g.cancel(&p.confirmation_id));
        assert!(g.confirm(&p.confirmation_id).await.is_err());
        assert_eq!(safe.load(Ordering::SeqCst), 0);
    }

    /// `caution` runs immediately and says so, rather than prompting.
    #[tokio::test]
    async fn a_caution_tool_runs_immediately_with_a_warning() {
        let (g, safe, _dir) = classified_gate();
        // Reclassify a *safe* tool as caution: it ran freely before and must
        // still run freely now, but carrying a warning.
        g.set_classification("probe.safe", Some(RiskLevel::Caution)).unwrap();
        let Outcome::Done { risk, warning, .. } = g.run("probe.safe", JsonMap::new(), Json::Null).await
        else {
            panic!("caution must not hold a call for confirmation")
        };
        assert_eq!(risk, RiskLevel::Caution);
        assert_eq!(safe.load(Ordering::SeqCst), 1);
        let w = warning.expect("a caution call must carry a warning for the UI");
        assert!(w.contains("caution"), "{w}");
    }

    /// A tool with no classification gets no warning: a warning on every call
    /// would be noise the user learns to ignore.
    #[tokio::test]
    async fn an_unclassified_tool_carries_no_warning() {
        let (g, _, _) = gate(&Config::default());
        let Outcome::Done { warning, risk, .. } = g.run("probe.safe", JsonMap::new(), Json::Null).await
        else {
            panic!()
        };
        assert!(warning.is_none(), "{warning:?}");
        assert_eq!(risk, RiskLevel::Safe);
    }

    /// `safe` is the other direction: the user trusts a tool the code calls
    /// dangerous, and it stops prompting.
    #[tokio::test]
    async fn a_dangerous_by_nature_tool_cannot_be_lowered() {
        let (g, _, danger) = gate(&Config::default());
        for l in [RiskLevel::Safe, RiskLevel::Caution] {
            let e = g.set_classification("probe.danger", Some(l)).unwrap_err();
            assert!(e.contains("cannot be lowered"), "{e}");
        }
        assert_eq!(g.classification("probe.danger"), None, "a refused change must not persist");
        assert!(matches!(
            g.run("probe.danger", JsonMap::new(), Json::Null).await,
            Outcome::NeedsConfirmation(_)
        ));
        assert_eq!(danger.load(Ordering::SeqCst), 0);
        // Raising it, or resetting it, is fine.
        g.set_classification("probe.danger", Some(RiskLevel::Dangerous)).unwrap();
        g.set_classification("probe.danger", None).unwrap();
    }

    /// Picking the built-in level clears the override instead of storing a
    /// no-op one that would show as "reclassified".
    #[tokio::test]
    async fn choosing_the_built_in_level_is_a_reset() {
        let (g, _, _) = gate(&Config::default());
        g.set_classification("probe.safe", Some(RiskLevel::Caution)).unwrap();
        let (_, overridden) = g.set_classification("probe.safe", Some(RiskLevel::Safe)).unwrap();
        assert!(!overridden);
        assert_eq!(g.classification("probe.safe"), None);
        // Also true for a dangerous-by-nature tool: picking dangerous is fine.
        assert!(!g.set_classification("probe.danger", Some(RiskLevel::Dangerous)).unwrap().1);
    }

    fn def(name: &str, steps: Json, params: &[&str]) -> arc_tools::custom::Def {
        arc_tools::custom::Def {
            name: name.into(),
            description: "test composite".into(),
            params: params.iter().map(|p| (p.to_string(), "x".to_string())).collect(),
            created: String::new(),
            fingerprint: String::new(),
            body: arc_tools::custom::Body::Composite { steps: serde_json::from_value(steps).unwrap() },
        }
    }

    fn judging_gate() -> (Gate, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let runs = Arc::new(AtomicUsize::new(0));
        let safe = Arc::new(AtomicUsize::new(0));
        let mut tools = Tools::new();
        tools.register(Arc::new(Judging { runs: runs.clone() }));
        tools.register(Arc::new(Probe { name: "probe.safe", risk: RiskLevel::Safe, runs: safe.clone() }));
        (Gate::new(Arc::new(tools), &Config::default()), runs, safe)
    }

    fn cmd(c: &str) -> JsonMap {
        [("c".to_string(), Json::String(c.into()))].into()
    }

    /// Wrapping a call in a composite changes nothing about whether it asks:
    /// each step is gated as if called directly, with the arguments filled in.
    #[tokio::test]
    async fn a_composite_gates_every_step_on_its_own() {
        let (g, runs, safe) = judging_gate();
        g.tools()
            .custom()
            .create(def(
                "tidy_up",
                serde_json::json!([{"tool": "probe.safe"}, {"tool": "probe.judging", "args": {"cmd": "{c}"}}, {"tool": "probe.safe"}]),
                &["c"],
            ))
            .unwrap();
        let Outcome::Done { result: ToolResult::Ok(v), .. } = g.run("tidy_up", cmd("ls"), Json::Null).await
        else {
            panic!("an ordinary chain runs")
        };
        assert_eq!(v["steps"].as_array().unwrap().len(), 3);
        assert_eq!((safe.load(Ordering::SeqCst), runs.load(Ordering::SeqCst)), (2, 1));

        // The dangerous step is held; the one before it ran, the one after did not.
        let Outcome::NeedsConfirmation(p) = g.run("tidy_up", cmd("boom"), Json::Null).await else {
            panic!("a dangerous step inside a composite must still be held")
        };
        assert_eq!(p.tool, "probe.judging", "the held call is the step itself, not the wrapper");
        assert!(
            p.explanation.contains("step 2 of 3") && p.explanation.contains("won't run"),
            "{}",
            p.explanation
        );
        assert_eq!((safe.load(Ordering::SeqCst), runs.load(Ordering::SeqCst)), (3, 1));
        assert!(g.cancel(&p.confirmation_id));
    }

    #[tokio::test]
    async fn a_composite_reports_the_step_that_failed_and_missing_args() {
        let (g, _, _) = judging_gate();
        let c = g.tools().custom();
        c.create(def(
            "needs_c",
            serde_json::json!([{"tool": "probe.judging", "args": {"cmd": "{c}"}}]),
            &["c"],
        ))
        .unwrap();
        let Outcome::Done { result: ToolResult::Error(e), .. } =
            g.run("needs_c", JsonMap::new(), Json::Null).await
        else {
            panic!()
        };
        assert!(e.contains("missing"), "{e}");
    }

    /// The composite's own row obeys the picker: raised to dangerous, the
    /// whole chain waits before any step runs, and approving it runs them.
    #[tokio::test]
    async fn a_composite_raised_to_dangerous_is_held_as_a_whole() {
        let (g, runs, safe) = judging_gate();
        g.tools().custom().create(def("chain", serde_json::json!([{"tool": "probe.safe"}]), &[])).unwrap();
        assert_eq!(g.base_risk_of("chain"), Some(RiskLevel::Safe), "worst of its steps");
        g.set_classification("chain", Some(RiskLevel::Dangerous)).unwrap();
        let Outcome::NeedsConfirmation(p) = g.run("chain", JsonMap::new(), Json::Null).await else {
            panic!()
        };
        assert_eq!(safe.load(Ordering::SeqCst), 0);
        assert!(matches!(
            g.confirm(&p.confirmation_id).await,
            Ok(Outcome::Done { result: ToolResult::Ok(_), .. })
        ));
        assert_eq!((safe.load(Ordering::SeqCst), runs.load(Ordering::SeqCst)), (1, 0));
    }

    /// Creating needs no confirmation; deleting always does, and a tool Arc
    /// did not make is refused rather than asked about.
    #[tokio::test]
    async fn creating_is_free_deleting_asks() {
        let (g, _, _) = judging_gate();
        let create: JsonMap = serde_json::from_value(serde_json::json!({
            "name": "say_hi", "description": "says hi", "kind": "script", "language": "bash", "script": "echo hi"
        }))
        .unwrap();
        assert!(matches!(
            g.run("tool_create", create, Json::Null).await,
            Outcome::Done { result: ToolResult::Ok(_), .. }
        ));
        assert!(g.tools().by_name("say_hi").is_some());
        assert_eq!(g.base_risk_of("say_hi"), Some(RiskLevel::Dangerous));
        // A script tool is dangerous by nature: every run asks, and the
        // picker cannot lower it.
        assert!(matches!(g.run("say_hi", JsonMap::new(), Json::Null).await, Outcome::NeedsConfirmation(_)));

        let del = |n: &str| -> JsonMap { [("name".to_string(), Json::String(n.into()))].into() };
        assert!(matches!(g.run("tool_delete", del("reboot"), Json::Null).await, Outcome::Denied { .. }));
        let Outcome::NeedsConfirmation(p) = g.run("tool_delete", del("say_hi"), Json::Null).await else {
            panic!()
        };
        assert!(g.tools().by_name("say_hi").is_some(), "nothing is deleted before the user says yes");
        assert!(g.set_classification("tool_delete", Some(RiskLevel::Safe)).is_err());
        g.confirm(&p.confirmation_id).await.unwrap();
        assert!(g.tools().by_name("say_hi").is_none());
    }

    /// A composite cannot smuggle tool management past the user.
    #[tokio::test]
    async fn a_composite_cannot_wrap_tool_management() {
        let (g, _, _) = judging_gate();
        let e = g.tools().custom().create(def(
            "sneaky",
            serde_json::json!([{"tool": "tool_delete", "args": {"name": "x"}}]),
            &[],
        ));
        assert!(e.unwrap_err().contains("cannot create"));
    }

    fn script_def(name: &str, text: &str) -> arc_tools::custom::Def {
        arc_tools::custom::Def {
            name: name.into(),
            description: "test script".into(),
            params: Default::default(),
            created: String::new(),
            fingerprint: String::new(),
            body: arc_tools::custom::Body::Script {
                language: arc_tools::custom::Language::Bash,
                script: text.into(),
            },
        }
    }

    /// The user asked for this: a self-made script they have reviewed can be
    /// set to safe, and then runs without a prompt.
    #[tokio::test]
    async fn a_reviewed_self_made_script_can_be_set_to_safe() {
        let (g, _, _) = judging_gate();
        g.tools().custom().create(script_def("say_hi", "echo hi")).unwrap();
        assert!(matches!(g.run("say_hi", JsonMap::new(), Json::Null).await, Outcome::NeedsConfirmation(_)));
        for level in [RiskLevel::Caution, RiskLevel::Safe] {
            g.set_classification("say_hi", Some(level)).unwrap();
            assert_eq!(g.risk_of("say_hi", &JsonMap::new()), Some(level));
            let Outcome::Done { result: ToolResult::Ok(v), .. } =
                g.run("say_hi", JsonMap::new(), Json::Null).await
            else {
                panic!("a script marked {level} must run without asking")
            };
            assert_eq!(v["output"], "hi");
        }
        // Built-ins that are dangerous by nature are still locked.
        for t in ["reboot", "tool_delete"] {
            assert!(g.set_classification(t, Some(RiskLevel::Safe)).is_err(), "{t}");
        }
    }

    /// Deleting a script takes its classification with it, so a new script
    /// reusing the name starts back at "asks every run".
    #[tokio::test]
    async fn a_recreated_script_does_not_inherit_the_old_ones_safe() {
        let (g, _, _) = judging_gate();
        g.tools().custom().create(script_def("shot", "echo old")).unwrap();
        g.set_classification("shot", Some(RiskLevel::Safe)).unwrap();
        let del: JsonMap = [("name".to_string(), Json::String("shot".into()))].into();
        let Outcome::NeedsConfirmation(p) = g.run("tool_delete", del, Json::Null).await else { panic!() };
        g.confirm(&p.confirmation_id).await.unwrap();
        assert_eq!(g.classification("shot"), None);
        g.tools().custom().create(script_def("shot", "echo new")).unwrap();
        assert!(matches!(g.run("shot", JsonMap::new(), Json::Null).await, Outcome::NeedsConfirmation(_)));
    }

    /// A script edited on disk after it was lowered loses the classification
    /// at the next load: the user approved the old text, not the new one.
    #[tokio::test]
    async fn an_edited_script_loses_its_classification_on_reload() {
        let dir = tempfile::tempdir().unwrap();
        let mk = || {
            let mut tools = Tools::new();
            tools.register(Arc::new(Probe {
                name: "probe.safe",
                risk: RiskLevel::Safe,
                runs: Arc::new(AtomicUsize::new(0)),
            }));
            let classes = ClassifiedTools::load_from(dir.path().join("classes.json"));
            let g = Gate::with_classes(Arc::new(tools), &Config::default(), classes);
            g.tools().custom().attach_dir(dir.path().join("tools"));
            g.forget_classifications(&g.tools().custom().changed_since_created());
            g
        };
        let g = mk();
        g.tools().custom().create(script_def("shot", "echo one")).unwrap();
        g.tools().custom().create(script_def("other", "echo two")).unwrap();
        g.set_classification("shot", Some(RiskLevel::Safe)).unwrap();
        g.set_classification("other", Some(RiskLevel::Safe)).unwrap();
        drop(g);
        // Unchanged: kept across a restart.
        assert_eq!(mk().classification("shot"), Some(RiskLevel::Safe));
        std::fs::write(dir.path().join("tools/shot/run.sh"), "echo one\necho sneaky\n").unwrap();
        let g = mk();
        assert_eq!(g.classification("shot"), None, "edited: back to asking");
        assert_eq!(g.classification("other"), Some(RiskLevel::Safe), "untouched: kept");
        assert!(matches!(g.run("shot", JsonMap::new(), Json::Null).await, Outcome::NeedsConfirmation(_)));
    }

    /// A tool whose risk depends on the call, like shell_exec.
    struct Judging {
        runs: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for Judging {
        fn name(&self) -> &str {
            "probe.judging"
        }
        fn description(&self) -> &str {
            "test probe"
        }
        fn base_risk(&self) -> RiskLevel {
            RiskLevel::Caution
        }
        fn assess(&self, args: &JsonMap) -> arc_tools::Assessment {
            let bad = args.get("cmd").and_then(|v| v.as_str()) == Some("boom");
            arc_tools::Assessment::new(if bad { RiskLevel::Dangerous } else { RiskLevel::Caution }, "probe")
        }
        async fn execute(&self, _: &JsonMap) -> ToolResult {
            self.runs.fetch_add(1, Ordering::SeqCst);
            ToolResult::Ok(Json::Null)
        }
    }

    /// Lowering a tool quiets its ordinary calls and nothing else: a call the
    /// tool itself rates dangerous still waits for the click.
    #[tokio::test]
    async fn lowering_a_tool_never_lowers_a_call_it_rates_dangerous() {
        let mut cfg = Config::default();
        cfg.permissions.confirm_at = RiskLevel::Caution;
        let runs = Arc::new(AtomicUsize::new(0));
        let mut tools = Tools::new();
        tools.register(Arc::new(Judging { runs: runs.clone() }));
        let g = Gate::new(Arc::new(tools), &cfg);
        let arg = |c: &str| {
            let mut m = JsonMap::new();
            m.insert("cmd".into(), Json::String(c.into()));
            m
        };
        assert!(matches!(g.run("probe.judging", arg("ls"), Json::Null).await, Outcome::NeedsConfirmation(_)));
        g.set_classification("probe.judging", Some(RiskLevel::Safe)).unwrap();
        assert!(matches!(g.run("probe.judging", arg("ls"), Json::Null).await, Outcome::Done { .. }));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        let Outcome::NeedsConfirmation(p) = g.run("probe.judging", arg("boom"), Json::Null).await else {
            panic!("a call the tool rates dangerous must still be held")
        };
        assert_eq!(p.risk, RiskLevel::Dangerous);
        assert_eq!(g.risk_of("probe.judging", &arg("boom")), Some(RiskLevel::Dangerous));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "nothing dangerous ran");
        assert!(g.cancel(&p.confirmation_id));
    }

    /// The regression, against the real shell_exec: nothing here executes,
    /// because every call must come back held or denied. If one does not,
    /// the test fails before anything runs -- risk_of is checked first.
    #[tokio::test]
    async fn real_shell_keeps_destructive_commands_held_when_lowered() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.permissions.shell.enabled = true;
        let tools = Arc::new(Tools::build(&cfg, None, "https://x/?q={query}".into()).unwrap());
        let g = Gate::with_classes(tools, &cfg, ClassifiedTools::load_from(dir.path().join("c.json")));
        for level in [RiskLevel::Caution, RiskLevel::Safe] {
            g.set_classification("shell_exec", Some(level)).unwrap();
            for cmd in ["systemctl poweroff", "rm -rf /tmp/arc-test-never-exists", "reboot"] {
                let mut a = JsonMap::new();
                a.insert("command".into(), Json::String(cmd.into()));
                assert_eq!(
                    g.risk_of("shell_exec", &a),
                    Some(RiskLevel::Dangerous),
                    "`{cmd}` must stay dangerous with shell_exec marked {level}"
                );
                match g.run("shell_exec", a, Json::Null).await {
                    Outcome::NeedsConfirmation(p) => assert!(g.cancel(&p.confirmation_id)),
                    Outcome::Denied { .. } => {}
                    Outcome::Done { .. } => panic!("`{cmd}` ran unasked with shell_exec marked {level}"),
                }
            }
        }
    }

    /// The setting has to outlive the process, or the dropdown is decoration.
    #[tokio::test]
    async fn a_classification_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tool_classes.json");

        {
            let (g, _, _) = gate_with(&Config::default(), ClassifiedTools::load_from(file.clone()));
            g.set_classification("probe.safe", Some(RiskLevel::Dangerous)).unwrap();
        }

        // Fresh process: fresh gate, same file.
        let (g2, safe, _) = gate_with(&Config::default(), ClassifiedTools::load_from(file));
        assert_eq!(g2.classification("probe.safe"), Some(RiskLevel::Dangerous));
        let Outcome::NeedsConfirmation(p) = g2.run("probe.safe", JsonMap::new(), Json::Null).await else {
            panic!("the classification did not survive the restart")
        };
        assert_eq!(safe.load(Ordering::SeqCst), 0);
        assert!(g2.confirm(&p.confirmation_id).await.is_ok());
        assert_eq!(safe.load(Ordering::SeqCst), 1);
    }

    /// Clearing the classification puts the tool back to what the code says.
    #[tokio::test]
    async fn clearing_a_classification_restores_the_built_in_level() {
        let (g, safe, _) = gate(&Config::default());
        g.set_classification("probe.safe", Some(RiskLevel::Dangerous)).unwrap();
        assert!(matches!(
            g.run("probe.safe", JsonMap::new(), Json::Null).await,
            Outcome::NeedsConfirmation(_)
        ));
        g.set_classification("probe.safe", None).unwrap();
        assert_eq!(g.classification("probe.safe"), None);
        assert!(matches!(g.run("probe.safe", JsonMap::new(), Json::Null).await, Outcome::Done { .. }));
        assert_eq!(safe.load(Ordering::SeqCst), 1);
    }

    /// A classification is not a permission. `rm -rf /` is refused by the shell
    /// analyzer whatever the dropdown says, and marking the tool dangerous
    /// must not turn a refusal into a prompt the user can click through.
    #[tokio::test]
    async fn a_classification_cannot_unblock_a_blocked_call() {
        let (g, _, _) = gate(&Config::default());
        g.set_classification("shell_exec", Some(RiskLevel::Safe)).unwrap();
        let mut a = JsonMap::new();
        a.insert("command".into(), Json::String("rm -rf /".into()));
        assert!(
            matches!(g.run("shell_exec", a, Json::Null).await, Outcome::Denied { .. }),
            "a blocked command must stay blocked when the tool is marked safe"
        );
    }

    /// Nor can it rescue a tool the user has disabled outright.
    #[tokio::test]
    async fn a_disabled_tool_stays_disabled_when_marked_dangerous() {
        let mut cfg = Config::default();
        cfg.tools.disabled = vec!["probe.safe".into()];
        let (g, safe, _) = gate(&cfg);
        g.set_classification("probe.safe", Some(RiskLevel::Dangerous)).unwrap();
        assert!(matches!(g.run("probe.safe", JsonMap::new(), Json::Null).await, Outcome::Denied { .. }));
        assert_eq!(safe.load(Ordering::SeqCst), 0);
    }

    /// A `deny` rule is the user saying "never", and it outranks a click.
    #[tokio::test]
    async fn a_deny_rule_outranks_a_safe_classification() {
        let mut cfg = Config::default();
        cfg.permissions.tools.insert("probe.safe".into(), arc_config::ToolPolicy::Deny);
        let (g, safe, _) = gate(&cfg);
        g.set_classification("probe.safe", Some(RiskLevel::Safe)).unwrap();
        assert!(matches!(g.run("probe.safe", JsonMap::new(), Json::Null).await, Outcome::Denied { .. }));
        assert_eq!(safe.load(Ordering::SeqCst), 0);
    }

    /// One tool's classification must not leak into another's.
    #[tokio::test]
    async fn classifications_are_per_tool() {
        let (g, safe, danger) = gate(&Config::default());
        g.set_classification("probe.safe", Some(RiskLevel::Dangerous)).unwrap();
        assert!(matches!(
            g.run("probe.safe", JsonMap::new(), Json::Null).await,
            Outcome::NeedsConfirmation(_)
        ));
        assert_eq!(safe.load(Ordering::SeqCst), 0);
        assert!(
            matches!(g.run("probe.danger", JsonMap::new(), Json::Null).await, Outcome::NeedsConfirmation(_)),
            "probe.danger is Dangerous in code and must still ask"
        );
        assert_eq!(danger.load(Ordering::SeqCst), 0);
    }

    /// The dropdown may only classify tools that exist.
    #[test]
    fn an_unknown_tool_cannot_be_classified() {
        let (g, _, _) = gate(&Config::default());
        let err = g.set_classification("nope", Some(RiskLevel::Dangerous)).unwrap_err();
        assert!(err.contains("unknown tool"), "{err}");
    }

    /// A real tool, not just a test probe: `window_list` is Safe in code, and
    /// marking it dangerous has to hold a real registry entry.
    #[tokio::test]
    async fn a_real_tool_can_be_classified_dangerous() {
        let (g, _, _) = gate(&Config::default());
        g.set_classification("window_list", Some(RiskLevel::Dangerous)).unwrap();
        assert_eq!(g.risk_of("window_list", &JsonMap::new()), Some(RiskLevel::Dangerous));
        // It is held, and cancelling means nothing happened -- which matters
        // because confirming would really enumerate the user's windows.
        let Outcome::NeedsConfirmation(p) = g.run("window_list", JsonMap::new(), Json::Null).await else {
            panic!("window_list must be held once classified dangerous")
        };
        assert!(g.cancel(&p.confirmation_id));
    }
}
