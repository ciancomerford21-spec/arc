//! The language-model agent loop: model → tool calls → gate → results →
//! model, until the model answers in text, a call needs confirmation, or
//! `ai.max_tool_rounds` is reached. Tool calls from the model get no special
//! trust: they go through the same [`Gate`] as everything else.

use crate::gate::{Gate, Outcome};
use arc_ai::{AiError, AiMessage, ProviderSet, ToolDef};
use arc_proto::{ActionRecord, PendingConfirmation};
use arc_tools::{JsonMap, ToolResult};
use serde_json::{Value as Json, json};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum AgentReply {
    /// Final text answer. `actions` records every tool call on the way
    /// (including denied ones).
    Answer { text: String, actions: Vec<ActionRecord> },
    /// Stopped because an action needs the user's confirmation.
    NeedsConfirmation { text: String, pending: PendingConfirmation, actions: Vec<ActionRecord> },
}

pub struct Agent {
    providers: ProviderSet,
    gate: Arc<Gate>,
    system_prompt: String,
    max_rounds: u32,
}

/// Tool results are truncated before going back to the model so a large
/// output (window list, shell output) can't blow the context window.
const MAX_RESULT_CHARS: usize = 4000;

fn result_for_model(outcome: &Outcome) -> String {
    let s = match outcome {
        Outcome::Done { result: ToolResult::Ok(v), .. } => json!({"ok": v}).to_string(),
        Outcome::Done { result: ToolResult::Error(e), .. } => json!({"error": e}).to_string(),
        Outcome::Denied { reason, .. } => json!({"denied": reason}).to_string(),
        Outcome::NeedsConfirmation(p) => json!({
            "awaiting_confirmation": p.explanation,
            "note": "The user must confirm this action. Tell them what you are waiting for.",
        })
        .to_string(),
    };
    if s.chars().count() > MAX_RESULT_CHARS {
        let mut t: String = s.chars().take(MAX_RESULT_CHARS).collect();
        t.push_str("…[truncated]");
        t
    } else {
        s
    }
}

impl Agent {
    pub fn new(providers: ProviderSet, gate: Arc<Gate>, system_prompt: String, max_rounds: u32) -> Self {
        Self { providers, gate, system_prompt, max_rounds: max_rounds.max(1) }
    }

    /// The tool schemas to send for this utterance. See
    /// `Tools::select_specs` for why this is not the full list.
    fn tool_defs_for(&self, utterance: &str) -> Vec<ToolDef> {
        self.gate
            .tools()
            .select_specs(utterance)
            .into_iter()
            .map(|s| ToolDef { name: s.name, description: s.description, parameters: s.parameters })
            .collect()
    }

    /// Answer `user` given prior `history` (without system prompt).
    pub async fn ask(&self, history: &[AiMessage], user: &str) -> Result<AgentReply, AiError> {
        let tools = self.tool_defs_for(user);
        let mut msgs = Vec::with_capacity(history.len() + 2);
        msgs.push(AiMessage::system(self.system_prompt.clone()));
        msgs.extend_from_slice(history);
        msgs.push(AiMessage::user(user));
        let mut actions = vec![];
        let mut failures = 0u32;

        for _ in 0..self.max_rounds {
            let r = self.providers.complete(&msgs, &tools).await?;
            if r.tool_calls.is_empty() {
                // Reword only a real answer. Confirmation prompts are built
                // below and spoken verbatim, and a failed round should not be
                // dressed up before the user is told what went wrong.
                let text = self.providers.phrase(&msgs, &r.content).await;
                return Ok(AgentReply::Answer { text, actions });
            }
            // Two rounds of failed calls in a row: stop letting the model guess
            // and have it explain instead (one final call, no tools).
            if failures >= 2 {
                msgs.push(AiMessage::assistant(r.content.clone(), vec![]));
                msgs.push(AiMessage::user(
                    "[system] Stop calling tools. In one short spoken sentence, tell the user what failed and \
                     what they could say instead.",
                ));
                let r = self.providers.complete(&msgs, &[]).await?;
                let text =
                    if r.content.trim().is_empty() { "Sorry, I couldn't do that.".into() } else { r.content };
                return Ok(AgentReply::Answer { text, actions });
            }
            msgs.push(AiMessage::assistant(r.content.clone(), r.tool_calls.clone()));
            let mut held: Option<PendingConfirmation> = None;
            for call in &r.tool_calls {
                let outcome = if held.is_some() {
                    // Don't run anything after a held action in the same turn.
                    Outcome::Denied {
                        tool: call.name.clone(),
                        reason: "skipped: an earlier action awaits confirmation".into(),
                        args: call.args.clone(),
                        risk: arc_proto::RiskLevel::Safe,
                    }
                } else {
                    let args: JsonMap = match &call.args {
                        Json::Object(m) => m.clone().into_iter().collect(),
                        _ => JsonMap::new(),
                    };
                    self.gate.run(&call.name, args, json!({"source": "ai"})).await
                };
                if let Outcome::NeedsConfirmation(p) = &outcome {
                    held = Some(p.clone());
                }
                actions.push(outcome.record());
                msgs.push(AiMessage::tool_result(call.id.clone(), result_for_model(&outcome)));
            }
            if let Some(pending) = held {
                let text = if r.content.trim().is_empty() {
                    format!("I need your confirmation to {}.", pending.explanation)
                } else {
                    r.content
                };
                return Ok(AgentReply::NeedsConfirmation { text, pending, actions });
            }
            let round_failed = actions
                .iter()
                .rev()
                .take(r.tool_calls.len())
                .all(|a| !matches!(a.outcome, arc_proto::ActionOutcome::Success));
            failures = if round_failed { failures + 1 } else { 0 };
        }
        Ok(AgentReply::Answer {
            text: "I stopped after too many steps without finishing. Try asking more specifically.".into(),
            actions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_ai::{AiResult, Provider, ToolCall};
    use arc_config::Config;
    use arc_proto::ActionOutcome;
    use arc_tools::Tools;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Replays scripted responses and records what it was sent.
    struct Scripted {
        replies: Mutex<Vec<AiResult>>,
        seen: Arc<Mutex<Vec<Vec<AiMessage>>>>,
    }

    #[async_trait]
    impl Provider for Scripted {
        fn name(&self) -> &str {
            "scripted"
        }
        async fn complete(&self, m: &[AiMessage], _: &[ToolDef]) -> Result<AiResult, AiError> {
            self.seen.lock().unwrap().push(m.to_vec());
            let mut r = self.replies.lock().unwrap();
            if r.is_empty() {
                Ok(AiResult { content: "loop".into(), tool_calls: vec![call("x", "network_status")] })
            } else {
                Ok(r.remove(0))
            }
        }
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall { id: id.into(), name: name.into(), args: json!({}) }
    }

    fn agent(replies: Vec<AiResult>, rounds: u32) -> (Agent, Arc<Mutex<Vec<Vec<AiMessage>>>>) {
        agent_with(replies, rounds, None)
    }

    fn agent_with(
        replies: Vec<AiResult>,
        rounds: u32,
        phrasing: Option<Box<dyn Provider>>,
    ) -> (Agent, Arc<Mutex<Vec<Vec<AiMessage>>>>) {
        let seen = Arc::new(Mutex::new(vec![]));
        let p = Scripted { replies: Mutex::new(replies), seen: seen.clone() };
        let gate = Arc::new(Gate::new(Arc::new(Tools::new()), &Config::default()));
        let set = ProviderSet::new(Box::new(p), None);
        let set = match phrasing {
            Some(ph) => set.with_phrasing(ph),
            None => set,
        };
        (Agent::new(set, gate, "sys".into(), rounds), seen)
    }

    /// A phrasing provider that always returns the same rewrite.
    struct Fixed(&'static str);

    #[async_trait]
    impl Provider for Fixed {
        fn name(&self) -> &str {
            "phraser"
        }
        async fn complete(&self, _: &[AiMessage], _: &[ToolDef]) -> Result<AiResult, AiError> {
            Ok(AiResult { content: self.0.into(), tool_calls: vec![] })
        }
    }

    /// A phrasing provider that always fails, to check the answer survives.
    struct Broken;

    #[async_trait]
    impl Provider for Broken {
        fn name(&self) -> &str {
            "broken"
        }
        async fn complete(&self, _: &[AiMessage], _: &[ToolDef]) -> Result<AiResult, AiError> {
            Err(AiError::Network("down".into()))
        }
    }

    #[tokio::test]
    async fn final_answer_is_reworded_by_the_phrasing_model() {
        let (a, _) = agent_with(
            vec![AiResult {
                content: "Volume has been set to 40 percent successfully.".into(),
                tool_calls: vec![],
            }],
            4,
            Some(Box::new(Fixed("Set it to forty."))),
        );
        let AgentReply::Answer { text, .. } = a.ask(&[], "set volume 40").await.unwrap() else { panic!() };
        assert_eq!(text, "Set it to forty.");
    }

    #[tokio::test]
    async fn phrasing_failure_keeps_the_original_answer() {
        // The phraser is an enhancement; losing it must never lose the reply.
        let (a, _) = agent_with(
            vec![AiResult { content: "Memory usage is at sixty two percent.".into(), tool_calls: vec![] }],
            4,
            Some(Box::new(Broken)),
        );
        let AgentReply::Answer { text, .. } = a.ask(&[], "memory?").await.unwrap() else { panic!() };
        assert_eq!(text, "Memory usage is at sixty two percent.");
    }

    #[tokio::test]
    async fn short_replies_are_not_reworded() {
        // "Cancelled." and similar are built elsewhere or are error paths;
        // rewording them would be noise.
        let (a, _) = agent_with(
            vec![AiResult { content: "Done.".into(), tool_calls: vec![] }],
            4,
            Some(Box::new(Fixed("something else entirely"))),
        );
        let AgentReply::Answer { text, .. } = a.ask(&[], "mute").await.unwrap() else { panic!() };
        assert_eq!(text, "Done.");
    }

    #[tokio::test]
    async fn confirmation_prompts_are_not_reworded() {
        // The prompt must name the exact action; a rewrite could drop it.
        let (a, _) = agent_with(
            vec![AiResult { content: String::new(), tool_calls: vec![call("c1", "reboot")] }],
            4,
            Some(Box::new(Fixed("Should I restart now?"))),
        );
        let AgentReply::NeedsConfirmation { text, pending, .. } = a.ask(&[], "reboot").await.unwrap() else {
            panic!()
        };
        assert_eq!(pending.tool, "reboot");
        assert!(text.contains("confirmation"), "{text}");
        assert!(!text.contains("restart now"), "confirmation text was reworded");
    }

    #[tokio::test]
    async fn no_phrasing_model_means_no_extra_call() {
        let (a, seen) =
            agent(vec![AiResult { content: "A perfectly ordinary answer.".into(), tool_calls: vec![] }], 4);
        let AgentReply::Answer { text, .. } = a.ask(&[], "hello").await.unwrap() else { panic!() };
        assert_eq!(text, "A perfectly ordinary answer.");
        assert_eq!(seen.lock().unwrap().len(), 1, "phrasing added a request");
    }

    #[tokio::test]
    async fn plain_answer() {
        let (a, _) = agent(vec![AiResult { content: "42".into(), tool_calls: vec![] }], 4);
        let AgentReply::Answer { text, actions } = a.ask(&[], "meaning?").await.unwrap() else { panic!() };
        assert_eq!(text, "42");
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn tool_result_is_fed_back() {
        let (a, seen) = agent(
            vec![
                AiResult { content: String::new(), tool_calls: vec![call("c1", "no_such_tool")] },
                AiResult { content: "done".into(), tool_calls: vec![] },
            ],
            4,
        );
        let AgentReply::Answer { text, .. } = a.ask(&[], "do it").await.unwrap() else { panic!() };
        assert_eq!(text, "done");
        let second = &seen.lock().unwrap()[1];
        let last = second.last().unwrap();
        assert_eq!(last.tool_call_id.as_deref(), Some("c1"));
        assert!(last.content.contains("denied"));
    }

    #[tokio::test]
    async fn model_cannot_reboot_without_confirmation() {
        let (a, seen) = agent(
            vec![AiResult {
                content: String::new(),
                tool_calls: vec![call("c1", "reboot"), call("c2", "shutdown")],
            }],
            4,
        );
        let AgentReply::NeedsConfirmation { pending, actions, .. } = a.ask(&[], "restart").await.unwrap()
        else {
            panic!("expected confirmation")
        };
        assert_eq!(pending.tool, "reboot");
        assert!(actions.iter().all(|r| r.outcome != ActionOutcome::Success), "nothing may run");
        assert_eq!(actions[0].outcome, ActionOutcome::AwaitingConfirmation);
        // Loop stopped: the model was called exactly once.
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn round_limit_stops_loop() {
        let (a, seen) = agent(vec![], 2);
        let AgentReply::Answer { text, .. } = a.ask(&[], "x").await.unwrap() else { panic!() };
        assert!(text.contains("too many steps"));
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn repeated_failures_stop_early_with_explanation() {
        // The model keeps calling a tool that fails; after two failed rounds it must
        // be asked (without tools) to explain instead of burning every round.
        let failing = || AiResult { content: String::new(), tool_calls: vec![call("f", "no_such_tool")] };
        let (a, seen) = agent(
            vec![
                failing(),
                failing(),
                failing(),
                AiResult { content: "That app isn't installed.".into(), tool_calls: vec![] },
            ],
            8,
        );
        let AgentReply::Answer { text, actions } = a.ask(&[], "open thunar").await.unwrap() else { panic!() };
        assert_eq!(text, "That app isn't installed.");
        assert_eq!(actions.len(), 2, "only two failed rounds executed");
        assert_eq!(seen.lock().unwrap().len(), 4);
        let last = seen.lock().unwrap().last().unwrap().clone();
        assert!(last.last().unwrap().content.contains("Stop calling tools"));
    }
}
