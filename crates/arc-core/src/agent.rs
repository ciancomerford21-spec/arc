//! The language-model agent loop: model → tool calls → gate → results →
//! model, until the model answers in text, a call needs confirmation, or
//! `ai.max_tool_rounds` is reached. Tool calls from the model get no special
//! trust: they go through the same [`Gate`] as everything else.

use crate::gate::{Gate, Outcome};
use arc_ai::{AiError, AiMessage, ProviderSet, ToolDef};

// Re-exported so the daemon can read provider health without taking a direct
// dependency on arc-ai just for one enum.
pub use arc_ai::LastCall;
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

/// Where the agent reports each step of a turn as it happens. The daemon
/// points this at its event bus so the Arc app can draw the thought process
/// live rather than reconstructing it after the reply.
pub type Trace = Arc<dyn Fn(arc_proto::Event) + Send + Sync>;

pub struct Agent {
    providers: ProviderSet,
    gate: Arc<Gate>,
    system_prompt: String,
    max_rounds: u32,
    trace: Option<Trace>,
}

/// Tool results are truncated before going back to the model so a large
/// output (window list, shell output) can't blow the context window.
const MAX_RESULT_CHARS: usize = 4000;

/// Longest reply Arc will speak before shortening it, in characters.
///
/// Kokoro is real-time, so this is also roughly the longest the user waits
/// before they can interrupt. 240 characters is about 110 seconds of speech,
/// which is already generous for a spoken answer; the median measured reply
/// before this was 274.
const MAX_SPOKEN_CHARS: usize = 240;

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

/// Keep a spoken reply to something a person will actually sit through.
///
/// The prompt asks for a word budget and the model mostly ignores it: measured
/// over five questions it wrote a median 274 characters whatever the cap said,
/// and lowering max_tokens only cut answers off mid-word. Neither works, because
/// the model reasons first and the budget lands after the reasoning.
///
/// So enforce it in software. Kokoro runs at real-time factor 1.0 (130 chars of
/// text -> 7.33s of audio), so characters ARE seconds of silence before the user
/// can barge in: 274 characters is 124 seconds of Arc talking. Keeping the
/// leading sentences gets the answer out first and relies on the model leading
/// with its conclusion, which it does.
///
/// The cut is announced rather than silent, so a shortened answer does not read
/// as a whole one.
/// Whether the dot at `hit` belongs to a URL rather than ending a sentence.
///
/// The tell is what surrounds it: a URL's dot is preceded by a domain label
/// and the whole run is under a scheme. Once seen, every later dot in that run
/// is also a URL dot, so this is checked before the generic "not followed by a
/// space" rule that would otherwise stop at the TLD.
fn is_url_context(rest: &str, hit: usize) -> bool {
    if !rest[..hit].contains("://") {
        return false;
    }
    // No sentence break between the scheme and this dot, i.e. the URL run is
    // unbroken. A space would mean the URL ended and a new sentence began.
    !rest[..hit].contains(' ')
}

pub fn shorten_for_speech(text: &str, max_chars: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= max_chars {
        return t.to_string();
    }
    let mut kept = String::new();
    let mut rest = t;
    // At least the first sentence always goes out, even when it alone
    // exceeds the budget. Dropping it would leave nothing to say, and a
    // too-long opener beats silence.
    let mut first = true;
    while !rest.is_empty() {
        // A sentence ends at . ! ? -- but not a decimal point or "e.g." style
        // fragment, which are followed by a non-space.
        // Scan forward for a real terminator, skipping any that is not one.
        // A terminator is a . ! ? followed by whitespace or end of text, and
        // not a decimal point between two digits. "Your load is 11.5 and the
        // CPU is zero." is the case that matters: cutting at the dot made Arc
        // open with "5 and the CPU is zero".
        // `end` is only meaningful once a real terminator is found; the scan
        // skips dots that are not terminators, so it needs its own "found
        // nothing" answer rather than reusing the running index.
        let mut scan = 0usize;
        let mut end = None;
        loop {
            let hit = match rest[scan..].find(|c| c == '.' || c == '!' || c == '?') {
                Some(i) => scan + i,
                None => break,
            };
            let after_dot = hit + 1;
            // A URL first: its dots are never sentence ends, and the generic
            // check below cannot tell "example.com" from "the end. Next".
            if is_url_context(rest, hit) {
                scan = after_dot;
                continue;
            }
            if after_dot < rest.len() && !rest[after_dot..].starts_with(char::is_whitespace) {
                scan = after_dot; // an abbreviation or file name
                continue;
            }
            let before = rest[..hit].chars().next_back();
            let after = rest[after_dot..].chars().next();
            if before.is_some_and(|b| b.is_ascii_digit()) && after.is_some_and(|a| a.is_ascii_digit()) {
                scan = after_dot; // a decimal such as 11.5
                continue;
            }
            end = Some(after_dot);
            break;
        }
        // No terminator anywhere left: what remains is one unbroken run.
        let Some(end) = end else { break };
        if end == 0 {
            break;
        }
        let next = &rest[..end];
        if !first && kept.chars().count() + next.chars().count() + 6 > max_chars {
            break;
        }
        first = false;
        kept.push_str(next);
        kept.push(' ');
        rest = rest[end..].trim_start();
    }
    if kept.trim().is_empty() {
        // One unbroken run (a URL, a stack trace): cutting mid-token would be
        // worse than speaking it, and a URL is not really "too talkative".
        return t.to_string();
    }
    format!("{} Want the rest?", kept.trim())
}

impl Agent {
    pub fn new(providers: ProviderSet, gate: Arc<Gate>, system_prompt: String, max_rounds: u32) -> Self {
        Self { providers, gate, system_prompt, max_rounds: max_rounds.max(1), trace: None }
    }

    pub fn set_trace(&mut self, trace: Trace) {
        self.trace = Some(trace);
    }

    fn emit(&self, e: arc_proto::Event) {
        if let Some(t) = &self.trace {
            t(e);
        }
    }

    /// What the provider last did, for the health line in `arc status`.
    pub fn last_call(&self) -> LastCall {
        self.providers.last_call()
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

        for round in 0..self.max_rounds {
            let r = self.providers.complete(&msgs, &tools).await?;
            // Words the model wrote alongside tool calls are its working notes;
            // a final answer is already shown as the reply, so it is not
            // repeated here.
            let note = if r.tool_calls.is_empty() { String::new() } else { r.content.trim().to_string() };
            if !r.reasoning.is_empty() || !note.is_empty() {
                self.emit(arc_proto::Event::Thought {
                    round: round + 1,
                    reasoning: r.reasoning.clone(),
                    text: note,
                });
            }
            if r.tool_calls.is_empty() && r.content.trim().is_empty() {
                // Nothing to say, even after the provider's bigger-budget
                // retry. Silence reads as Arc having died -- the user asked
                // "why is your reply empty?" -- and the model, asked why, made
                // up a reason. Say what actually happened.
                let why = if r.truncated {
                    "I ran out of room thinking before I got to an answer. Ask again, or ask for something smaller."
                } else {
                    "I came back with nothing to say to that. Try asking again."
                };
                tracing::warn!(round, truncated = r.truncated, "model returned an empty reply");
                return Ok(AgentReply::Answer { text: why.into(), actions });
            }
            if r.tool_calls.is_empty() && claims_confirmation(&r.content) {
                // The model copied Arc's own confirmation prompt out of the
                // conversation history. Measured live: "Now recreate
                // screenshot tool" was answered "I need your confirmation to
                // delete my script tool `screenshot`" with nothing held --
                // the tool was already gone, so there was nothing the user
                // could approve. Only a real hold may say this.
                tracing::warn!(round, "model imitated a confirmation prompt with nothing held");
                msgs.push(AiMessage::assistant(r.content.clone(), vec![]));
                msgs.push(AiMessage::user(
                    "[system] Nothing is waiting for confirmation, so do not ask for one. \
                     Do what the user asked now by calling the right tool, or say plainly why you can't.",
                ));
                continue;
            }
            if r.tool_calls.is_empty() {
                // Reword only a real answer. Confirmation prompts are built
                // below and spoken verbatim, and a failed round should not be
                // dressed up before the user is told what went wrong.
                let text = self.providers.phrase(&msgs, &r.content).await;
                return Ok(AgentReply::Answer { text: shorten_for_speech(&text, MAX_SPOKEN_CHARS), actions });
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
                return Ok(AgentReply::Answer { text: shorten_for_speech(&text, MAX_SPOKEN_CHARS), actions });
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
                    self.emit(arc_proto::Event::ToolStarted {
                        tool: call.name.clone(),
                        args: call.args.clone(),
                        risk: self.gate.risk_of(&call.name, &args).unwrap_or(arc_proto::RiskLevel::Safe),
                    });
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

/// Text that asks the user to approve something. Only Arc's own gate may
/// produce this; from the model it is an imitation of an earlier turn.
fn claims_confirmation(text: &str) -> bool {
    let t = text.to_lowercase();
    ["need your confirmation", "confirm with: arc confirm", "needs your go-ahead", "need your go-ahead"]
        .iter()
        .any(|p| t.contains(p))
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
                Ok(AiResult {
                    content: "loop".into(),
                    tool_calls: vec![call("x", "network_status")],
                    reasoning: String::new(),
                    truncated: false,
                })
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
            Ok(AiResult {
                content: self.0.into(),
                tool_calls: vec![],
                reasoning: String::new(),
                truncated: false,
            })
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
                reasoning: String::new(),
                truncated: false,
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
            vec![AiResult {
                content: "Memory usage is at sixty two percent.".into(),
                tool_calls: vec![],
                reasoning: String::new(),
                truncated: false,
            }],
            4,
            Some(Box::new(Broken)),
        );
        let AgentReply::Answer { text, .. } = a.ask(&[], "memory?").await.unwrap() else { panic!() };
        assert_eq!(text, "Memory usage is at sixty two percent.");
    }

    #[tokio::test]
    async fn an_empty_reply_is_explained_not_spoken_as_silence() {
        let empty =
            |t| AiResult {
                content: String::new(), tool_calls: vec![], reasoning: "…".into(), truncated: t
            };
        for (cut, want) in [(true, "ran out of room"), (false, "nothing to say")] {
            // Two: the provider set retries a cut-off reply once with a
            // bigger budget, and here that comes back empty too.
            let (a, _) = agent(vec![empty(cut), empty(cut)], 4);
            let AgentReply::Answer { text, .. } = a.ask(&[], "list some tools").await.unwrap() else {
                panic!()
            };
            assert!(text.contains(want), "{text}");
        }
    }

    /// The model copying Arc's own "I need your confirmation" line with
    /// nothing held is sent back to act instead.
    #[tokio::test]
    async fn an_imitated_confirmation_prompt_is_not_passed_on() {
        let fake = AiResult {
            content: "I need your confirmation to delete my script tool `screenshot`.".into(),
            tool_calls: vec![],
            reasoning: String::new(),
            truncated: false,
        };
        let real = AiResult {
            content: "Made it.".into(),
            tool_calls: vec![],
            reasoning: String::new(),
            truncated: false,
        };
        let (a, seen) = agent(vec![fake, real], 4);
        let AgentReply::Answer { text, .. } = a.ask(&[], "recreate the screenshot tool").await.unwrap()
        else {
            panic!()
        };
        assert_eq!(text, "Made it.");
        let last = seen.lock().unwrap().last().unwrap().last().unwrap().content.clone();
        assert!(last.contains("Nothing is waiting for confirmation"), "{last}");
    }

    #[tokio::test]
    async fn short_replies_are_not_reworded() {
        // "Cancelled." and similar are built elsewhere or are error paths;
        // rewording them would be noise.
        let (a, _) = agent_with(
            vec![AiResult {
                content: "Done.".into(),
                tool_calls: vec![],
                reasoning: String::new(),
                truncated: false,
            }],
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
            vec![AiResult {
                content: String::new(),
                tool_calls: vec![call("c1", "reboot")],
                reasoning: String::new(),
                truncated: false,
            }],
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
        let (a, seen) = agent(
            vec![AiResult {
                content: "A perfectly ordinary answer.".into(),
                tool_calls: vec![],
                reasoning: String::new(),
                truncated: false,
            }],
            4,
        );
        let AgentReply::Answer { text, .. } = a.ask(&[], "hello").await.unwrap() else { panic!() };
        assert_eq!(text, "A perfectly ordinary answer.");
        assert_eq!(seen.lock().unwrap().len(), 1, "phrasing added a request");
    }

    #[tokio::test]
    async fn plain_answer() {
        let (a, _) = agent(
            vec![AiResult {
                content: "42".into(),
                tool_calls: vec![],
                reasoning: String::new(),
                truncated: false,
            }],
            4,
        );
        let AgentReply::Answer { text, actions } = a.ask(&[], "meaning?").await.unwrap() else { panic!() };
        assert_eq!(text, "42");
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn tool_result_is_fed_back() {
        let (a, seen) = agent(
            vec![
                AiResult {
                    content: String::new(),
                    tool_calls: vec![call("c1", "no_such_tool")],
                    reasoning: String::new(),
                    truncated: false,
                },
                AiResult {
                    content: "done".into(),
                    tool_calls: vec![],
                    reasoning: String::new(),
                    truncated: false,
                },
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
                reasoning: String::new(),
                truncated: false,
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
        let failing = || AiResult {
            content: String::new(),
            tool_calls: vec![call("f", "no_such_tool")],
            reasoning: String::new(),
            truncated: false,
        };
        let (a, seen) = agent(
            vec![
                failing(),
                failing(),
                failing(),
                AiResult {
                    content: "That app isn't installed.".into(),
                    tool_calls: vec![],
                    reasoning: String::new(),
                    truncated: false,
                },
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

    #[test]
    fn a_short_reply_is_untouched() {
        assert_eq!(shorten_for_speech("Done.", 240), "Done.");
        assert_eq!(shorten_for_speech("  padded  ", 240), "padded");
    }

    #[test]
    fn a_long_reply_keeps_the_leading_sentences_and_says_so() {
        let long = "A closure is a function that remembers the variables from where it \
                    was defined. That is the whole trick, and it is why closures are \
                    useful for callbacks. The variables stay alive after the outer \
                    function has returned.";
        let out = shorten_for_speech(long, 120);
        assert!(out.starts_with("A closure is a function"), "{out}");
        assert!(out.ends_with("Want the rest?"), "{out}");
        // Must not cut mid-word, and must actually be shorter.
        assert!(out.chars().count() < long.chars().count(), "{out}");
        assert!(out.chars().count() <= 120 + 6, "{} chars", out.chars().count());
    }

    #[test]
    fn a_decimal_in_the_opening_sentence_is_not_a_cut_point() {
        // The real failure: the model opened with a load average, and the
        // guard cut at the dot, so Arc spoke "5 and the CPU is still sitting at
        // zero" as the first thing the user heard.
        let text = "Your load average is 11.5 and the CPU is still sitting at zero. \
                    That gap means fifteen tasks are queued on something.";
        let out = shorten_for_speech(text, 90);
        assert!(out.starts_with("Your load average is 11.5"), "cut inside a decimal: {out:?}");
    }

    #[test]
    fn a_decimal_point_is_not_a_sentence_end() {
        // "3.5" cut at the dot would speak a fragment and then ask if they want
        // the rest of a number.
        let text = "Your load is 3.5 right now, which is fine. It was 11 earlier though.";
        let out = shorten_for_speech(text, 30);
        assert!(out.contains("3.5") || out.starts_with("Your load is 3.5"), "{out}");
    }

    #[test]
    fn one_unbroken_run_is_spoken_whole() {
        // A URL or a path has no sentence break. Cutting it would be worse than
        // the pause, and it is not the rambling this guard exists for.
        let url = "https://example.com/a/very/long/path/that/never/ends/and/keeps/going/forever";
        assert_eq!(shorten_for_speech(url, 60), url);
    }

    #[test]
    fn the_guard_actually_bounds_the_speech_time() {
        // The point of the guard. 240 characters is ~110s of speech at Kokoro's
        // measured real-time factor of 1.0.
        let huge = (0..40)
            .map(|i| format!("This is sentence number {i} and it goes on for a while."))
            .collect::<Vec<_>>()
            .join(" ");
        let out = shorten_for_speech(&huge, MAX_SPOKEN_CHARS);
        assert!(
            out.chars().count() <= MAX_SPOKEN_CHARS + 6,
            "{} chars is {}s of speech",
            out.chars().count(),
            out.chars().count() as f64 / 2.2
        );
    }

    /// The app draws the thought process from these events, so the agent must
    /// publish them in order: reasoning, then the tool it chose, before it runs.
    #[tokio::test]
    async fn each_step_is_traced_before_the_reply() {
        let (mut a, _) = agent(
            vec![
                AiResult {
                    content: "checking".into(),
                    tool_calls: vec![call("c1", "network_status")],
                    reasoning: "user wants network".into(),
                    truncated: false,
                },
                AiResult {
                    content: "All good.".into(),
                    tool_calls: vec![],
                    reasoning: String::new(),
                    truncated: false,
                },
            ],
            4,
        );
        let seen = Arc::new(Mutex::new(vec![]));
        let sink = seen.clone();
        a.set_trace(Arc::new(move |e| sink.lock().unwrap().push(e)));
        a.ask(&[], "is the network up").await.unwrap();
        let seen = seen.lock().unwrap();
        assert!(matches!(&seen[0], arc_proto::Event::Thought { round: 1, reasoning, text }
            if reasoning == "user wants network" && text == "checking"));
        assert!(matches!(&seen[1], arc_proto::Event::ToolStarted { tool, .. } if tool == "network_status"));
        // A plain final answer is the reply itself, not a thought.
        assert_eq!(seen.len(), 2, "{seen:?}");
    }
}
