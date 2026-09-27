//! User automations: phrases that run a fixed sequence of tool calls, loaded
//! from `~/.config/arc/automations.toml` (see `arc_config::automations`).
//!
//! Matching is exact after normalisation (case, punctuation, "please",
//! a leading "hey arc"), so a custom phrase never swallows unrelated requests.
//! Every tool step goes through the [`Gate`]: a step that needs confirmation
//! pauses the automation and the remaining steps are dropped (the user can
//! re-run it after approving).

use crate::gate::{Gate, Outcome};
use arc_config::automations::{Automation, AutomationFile, Step};
use arc_proto::ActionRecord;
use arc_tools::{JsonMap, ToolResult};
use serde_json::{Value as Json, json};
use std::time::Duration;

pub struct Automations {
    list: Vec<Automation>,
}

pub struct RunResult {
    pub text: String,
    pub actions: Vec<ActionRecord>,
    pub pending: Option<arc_proto::PendingConfirmation>,
}

pub fn norm_phrase(s: &str) -> String {
    let t: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' })
        .collect();
    let mut words: Vec<&str> = t.split_whitespace().collect();
    for lead in [&["hey", "arc"][..], &["ok", "arc"], &["okay", "arc"], &["arc"], &["please"]] {
        if words.len() > lead.len() && words[..lead.len()] == *lead {
            words.drain(..lead.len());
        }
    }
    if words.last() == Some(&"please") {
        words.pop();
    }
    words.join(" ")
}

impl Automations {
    pub fn new(file: AutomationFile) -> Self {
        Self { list: file.automations }
    }

    pub fn empty() -> Self {
        Self { list: vec![] }
    }

    pub fn list(&self) -> &[Automation] {
        &self.list
    }

    /// The automation whose name or trigger equals `text` (normalised).
    pub fn find(&self, text: &str) -> Option<&Automation> {
        let t = norm_phrase(text);
        if t.is_empty() {
            return None;
        }
        self.list.iter().find(|a| a.phrases().any(|p| norm_phrase(p) == t))
    }

    /// Check every tool step names a real tool. Returns human-readable problems.
    pub fn problems(&self, gate: &Gate) -> Vec<String> {
        let mut out = vec![];
        for a in &self.list {
            for s in &a.steps {
                if let Step::Tool { tool, .. } = s {
                    if gate.tools().by_name(tool).is_none() {
                        out.push(format!("automation \"{}\": unknown tool \"{tool}\" (see `arc tools`)", a.name));
                    }
                }
            }
        }
        out
    }

    pub async fn run(&self, a: &Automation, gate: &Gate) -> RunResult {
        let mut said: Vec<String> = vec![];
        let mut actions = vec![];
        for step in &a.steps {
            match step {
                Step::Wait { wait_ms } => tokio::time::sleep(Duration::from_millis((*wait_ms).min(60_000))).await,
                Step::Say { say } => said.push(say.clone()),
                Step::Tool { tool, args } => {
                    let args: JsonMap = match args {
                        Json::Object(m) => m.clone().into_iter().collect(),
                        _ => JsonMap::new(),
                    };
                    let outcome = gate.run(tool, args, json!({"source": "automation", "automation": a.name})).await;
                    actions.push(outcome.record());
                    match outcome {
                        Outcome::Done { result: ToolResult::Ok(_), .. } => {}
                        Outcome::NeedsConfirmation(p) => {
                            let text = format!(
                                "\"{}\" needs your confirmation to {}. Say yes to continue; the rest of the routine will then need to be run again.",
                                a.name, p.explanation
                            );
                            return RunResult { text, actions, pending: Some(p) };
                        }
                        Outcome::Done { result: ToolResult::Error(e), .. } => {
                            if !a.continue_on_error {
                                return RunResult { text: format!("\"{}\" stopped: {tool} failed: {e}", a.name), actions, pending: None };
                            }
                        }
                        Outcome::Denied { reason, .. } => {
                            if !a.continue_on_error {
                                return RunResult { text: format!("\"{}\" stopped: {tool} was refused: {reason}", a.name), actions, pending: None };
                            }
                        }
                    }
                }
            }
        }
        let text = if said.is_empty() { format!("Done: {}.", a.name) } else { said.join(" ") };
        RunResult { text, actions, pending: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_config::Config;
    use arc_tools::Tools;
    use std::sync::Arc;

    fn autos(toml: &str) -> Automations {
        Automations::new(arc_config::automations::parse(toml).unwrap())
    }

    #[test]
    fn phrases_match_exactly_after_normalising() {
        let a = autos("[[automation]]\nname = \"gaming mode\"\ntriggers = [\"let's play\"]\nsteps = [{ say = \"ok\" }]\n");
        for t in ["gaming mode", "Gaming mode.", "Hey Arc, gaming mode please", "let's play!", "arc let's play"] {
            assert!(a.find(t).is_some(), "{t}");
        }
        for t in ["gaming", "start gaming mode now", "play", ""] {
            assert!(a.find(t).is_none(), "{t}");
        }
    }

    #[tokio::test]
    async fn runs_steps_and_speaks_say_lines() {
        let a = autos("[[automation]]\nname = \"check\"\nsteps = [{ tool = \"power_info\" }, { wait_ms = 1 }, { say = \"All checked.\" }]\n");
        let gate = Gate::new(Arc::new(Tools::new()), &Config::default());
        let r = a.run(a.find("check").unwrap(), &gate).await;
        assert_eq!(r.text, "All checked.");
        assert_eq!(r.actions.len(), 1);
        assert!(r.pending.is_none());
    }

    #[tokio::test]
    async fn dangerous_step_pauses_for_confirmation() {
        let a = autos("[[automation]]\nname = \"bye\"\nsteps = [{ tool = \"reboot\" }, { say = \"never said\" }]\n");
        let gate = Gate::new(Arc::new(Tools::new()), &Config::default());
        let r = a.run(a.find("bye").unwrap(), &gate).await;
        assert!(r.pending.is_some(), "reboot must not run unconfirmed");
        assert!(!r.text.contains("never said"));
    }

    #[test]
    fn unknown_tools_are_reported() {
        let a = autos("[[automation]]\nname = \"x\"\nsteps = [{ tool = \"apps.launch\" }, { tool = \"app_launch\", args = { app = \"code\" } }]\n");
        let gate = Gate::new(Arc::new(Tools::new()), &Config::default());
        let p = a.problems(&gate);
        assert_eq!(p.len(), 1);
        assert!(p[0].contains("apps.launch"));
    }

    #[test]
    fn shipped_example_only_uses_real_tools() {
        let f = arc_config::automations::parse(include_str!("../../../config/automations.toml")).unwrap();
        let gate = Gate::new(Arc::new(Tools::new()), &Config::default());
        assert_eq!(Automations::new(f).problems(&gate), Vec::<String>::new());
    }
}
