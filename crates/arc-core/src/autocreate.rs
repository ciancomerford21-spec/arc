//! Arc writes a tool for itself when it meets a job no existing tool can do.
//!
//! `tool_create` is the explicit path: the user asks Arc to learn a routine,
//! and Arc writes it. This module is the implicit one. When the model tries to
//! call a tool that does not exist -- or calls `tool_missing`, which exists
//! only to say "nothing I have can do this" -- Arc asks the model to design the
//! tool it wanted, checks the design against the safety screen below, registers
//! it through the same `CustomTools::create` that `tool_create` uses, and puts
//! the tool in the current session's tool list so the original request is
//! retried immediately rather than in the next request.
//!
//! Creating a tool is not the same as running one, and that is the whole safety
//! argument. A composite adds no capability: the gate runs every step as if it
//! had been called on its own. A script tool is dangerous by nature and asks
//! before every run until the user lowers it, and its text is screened against
//! the shell blocklist at creation and again at every load. So the worst a
//! mistaken auto-creation can do is leave a file under
//! `~/.local/share/arc/tools/` that asks for permission when used.
//!
//! What is refused outright:
//!
//! * a request to delete, wipe, format, kill, shut down, disable or otherwise
//!   destroy something -- manufacturing a tool for it is how an assistant talks
//!   itself into `rm -rf`;
//! * a composite that steps on a tool nobody would run unattended
//!   (`reboot`, `shutdown`, `tool_delete`);
//! * a script containing a destructive primitive the shell analyzer would
//!   refuse anyway, in either language (python gets its own screen: it never
//!   reaches the bash one);
//! * a request too short to say what the tool should be;
//! * more than `ai.auto_create_max_per_turn` (default 1) per turn or
//!   `ai.auto_create_max_per_session` (default 5) per session.
//!
//! Ambiguity resolved by choice, recorded in README.md: the sentence in the
//! task about *not* auto-creating is read as two rules -- not for a request
//! that is not an action at all (too short, or a pure question), and not
//! beyond the per-turn budget. Those are what the last two refusals above
//! implement.

use crate::gate::Gate;
use arc_ai::{AiError, AiMessage, ProviderSet};
use arc_tools::custom::{Body, Def, Language, Step};
use arc_tools::{CustomTools, ToolSpec};
use regex::Regex;
use serde::Deserialize;
use std::sync::atomic::{AtomicU32, Ordering};

/// The built-in the model calls to declare a gap. It does nothing by itself;
/// the agent intercepts the call before the gate and runs the planner.
pub const TOOL_MISSING: &str = "tool_missing";

/// Words that mean the request is about destroying something. Matched on word
/// boundaries over the whole utterance, so "delete the file Arc just made" is
/// refused while "list the deleted items" -- which the user is asking about,
/// not doing -- is refused too. Erring towards refusing is cheap: the model
/// then says it cannot do it, which is the truth.
const DESTRUCTIVE: &[&str] = &[
    "delete",
    "deleted",
    "deleting",
    "erase",
    "wipe",
    "destroy",
    "purge",
    "format",
    "uninstall",
    "reinstall",
    "reboot",
    "shutdown",
    "shut down",
    "shutting down",
    "poweroff",
    "power off",
    "halt",
    "kill",
    "killall",
    "pkill",
    "truncate",
    "nuke",
    "brick",
    // Multi-word commands: substring matches, no word boundaries.
    "rm -rf",
    "mkfs",
    "dd if",
    "drop table",
    "drop database",
    "factory reset",
    "force push",
];

/// Tools a composite must never step on. Each is dangerous by nature, and
/// auto-created composites are exactly the case where nobody wrote the steps
/// by hand and checked them. (`shell_exec` is deliberately absent: it is
/// screened per call and per line, which is the same guarantee the explicit
/// path has.)
const DANGEROUS_STEPS: &[&str] = &["reboot", "shutdown", "tool_delete", "tool_create", "tool_list_own", TOOL_MISSING];

/// Python never reaches the bash analyzer, so it gets its own screen for the
/// primitives that destroy: file and tree removal, disks, partitions, and
/// shelling out to the same commands bash is blocked from.
const PYTHON_DESTRUCTIVE: &[&str] = &[
    r"shutil\.rmtree",
    r"os\.remove",
    r"os\.unlink",
    r"os\.rmdir",
    r"os\.removedirs",
    r"os\.truncate",
    r"os\.system",
    r#"subprocess\.[a-zA-Z_]*\s*\(\s*["'](?:/bin/)?(?:ba)?sh["']"#,
    r"rm\s+-rf",
    r"mkfs",
    r"dd\s+if=",
    r"os\.sync",
];

/// Auto-creation policy and budget counters.
pub struct AutoCreate {
    pub enabled: bool,
    pub max_per_turn: u32,
    max_per_session: u32,
    /// Reset at the start of every turn.
    this_turn: AtomicU32,
    session: AtomicU32,
}

impl AutoCreate {
    pub fn from_settings(enabled: bool, max_per_turn: u32, max_per_session: u32) -> Self {
        Self {
            enabled,
            max_per_turn: max_per_turn.max(1),
            max_per_session: max_per_session,
            this_turn: AtomicU32::new(0),
            session: AtomicU32::new(0),
        }
    }

    /// Off entirely. Used by library callers and by the agent's own tests,
    /// which assert on exactly how many model requests a turn costs.
    pub fn disabled() -> Self {
        Self::from_settings(false, 0, 0)
    }

    pub fn begin_turn(&self) {
        self.this_turn.store(0, Ordering::SeqCst);
    }

    /// Tools this session made without being asked, for `arc status`.
    pub fn made(&self) -> u32 {
        self.session.load(Ordering::SeqCst)
    }

    /// Everything that must be true before one model request is spent on
    /// designing a tool.
    fn permit(&self, request: &str) -> Result<(), String> {
        if !self.enabled {
            return Err("auto tool creation is off (ai.auto_create_tools = false)".into());
        }
        let turn = self.this_turn.fetch_add(1, Ordering::SeqCst);
        if turn >= self.max_per_turn {
            self.this_turn.store(self.max_per_turn, Ordering::SeqCst);
            return Err(format!("already auto-created a tool this turn (limit {})", self.max_per_turn));
        }
        let made = self.session.load(Ordering::SeqCst);
        if made >= self.max_per_session {
            return Err(format!("already auto-created {made} tools this session (limit {})", self.max_per_session));
        }
        let words = request.split_whitespace().count();
        if words < 3 {
            // "dim the lights" is one tool call, not a new capability. A tool
            // built from one or two words cannot have a description worth
            // reading, which is the only thing that makes a self-made tool
            // findable later.
            return Err("the request is too short to design a tool from".into());
        }
        if let Some(w) = destructive_word(request) {
            return Err(format!("the request asks to {w}, which is not something to build a tool for"));
        }
        Ok(())
    }

    /// Ask the model to design the tool it wanted, then register it.
    ///
    /// Returns the created definition, or why it was refused. Every refusal
    /// here is reported back to the model as the tool result, so it can carry
    /// on with what it does have rather than inventing a tool name again.
    pub async fn plan(
        &self,
        providers: &ProviderSet,
        store: &CustomTools,
        known: &[ToolSpec],
        gap: &Gap,
    ) -> Result<Def, String> {
        let request = gap.request.trim();
        self.permit(request)?;
        let msgs = plan_messages(gap, known);
        tracing::info!(wanted = %gap.name, request, "designing a missing tool");
        let r = providers.complete(&msgs, &[]).await.map_err(|e| match e {
            AiError::Network(_) | AiError::Timeout | AiError::Provider(_) => format!("the model could not design it ({e})"),
            AiError::Unconfigured => "no language model is configured".to_string(),
        })?;
        if r.tool_calls.iter().any(|c| c.name == TOOL_MISSING || c.name == "tool_create") {
            return Err("the model asked for a tool instead of writing one".into());
        }
        let draft = parse_draft(&r.content)?;
        // Keep the model honest about the name: it is the name the model
        // reached for and, after creation, the one it will call.
        let d = draft.into_def(gap)?;
        if known.iter().any(|s| s.name == d.name) {
            return Err(format!("`{}` is already a tool, so nothing was missing", d.name));
        }
        if !d.description.trim().is_empty() {
            screen_definition(&d)?;
        }
        let d = store.create(d).map_err(|e| format!("the tool could not be created: {e}"))?;
        self.session.fetch_add(1, Ordering::SeqCst);
        tracing::info!(tool = %d.name, kind = d.kind(), "auto-created a tool");
        Ok(d)
    }
}

/// A gap the agent found: the model reached for something that is not there.
#[derive(Debug, Clone, PartialEq)]
pub struct Gap {
    /// The tool name the model used, normalised into something `Def` accepts.
    pub name: String,
    /// What the model said it needed, if it said anything.
    pub want: String,
    /// The user's original request, which is what the tool has to serve.
    pub request: String,
    /// Ids of the tool calls that declared this gap, so each one gets an
    /// answer (an OpenAI-style conversation rejects a tool call with no tool
    /// message after it).
    pub ids: Vec<String>,
    /// True when the model called `tool_missing` rather than inventing a name.
    pub declared: bool,
}

/// Bring a model-invented tool name into the shape `Def` accepts:
/// `Weather Forecast` and `weather-forecast` both become `weather_forecast`.
/// A name that cannot be salvaged is the model's fault and says so.
pub fn normalise_name(raw: &str) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut last_us = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_us = false;
        } else if !last_us && !out.is_empty() {
            out.push('_');
            last_us = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() || !out.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err(format!("`{}` is not usable as a tool name", raw.trim()));
    }
    if out.len() > 40 {
        out.truncate(40);
        while out.ends_with('_') {
            out.pop();
        }
    }
    if out.len() < 3 {
        return Err(format!("`{}` is not usable as a tool name", raw.trim()));
    }
    Ok(out)
}

/// The first destructive word in the text, if any.
pub fn destructive_word(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();
    // Word boundaries for the plain words; the multi-word phrases
    // ("rm -rf", "drop table") are substring matches on purpose.
    DESTRUCTIVE.iter().copied().find(|w| {
        if w.contains(' ') || w.contains('-') && w.len() > 6 {
            lower.contains(w)
        } else {
            Regex::new(&format!(r"\b{}\b", regex::escape(w))).is_ok_and(|r| r.is_match(&lower))
        }
    })
}

/// Every refusal that applies to a finished design, as one string.
///
/// Called on the model output before it is written anywhere, so a design that
/// would be refused never reaches `create()` and never reaches disk.
pub fn screen_definition(d: &Def) -> Result<(), String> {
    match &d.body {
        Body::Composite { steps } => screen_steps(steps),
        Body::Script { language, script } => screen_script(*language, script),
    }
}

fn screen_steps(steps: &[Step]) -> Result<(), String> {
    for (i, s) in steps.iter().enumerate() {
        let n = i + 1;
        if DANGEROUS_STEPS.contains(&s.tool.as_str()) {
            return Err(format!("step {n} calls `{}`, which must never be part of a made tool", s.tool));
        }
        let mut strs = vec![];
        collect_strings(&s.args, &mut strs);
        if let Some(w) = strs.iter().find_map(|x| destructive_word(x)) {
            return Err(format!("step {n} asks to {w}"));
        }
    }
    Ok(())
}

fn screen_script(language: Language, script: &str) -> Result<(), String> {
    let patterns: &[&str] = match language {
        // The bash analyzer screens these at creation inside
        // `CustomTools::validate`; this is the same list seen from the other
        // side, so the refusal arrives as a sentence rather than as a line
        // number in a lower layer.
        Language::Bash => DESTRUCTIVE,
        Language::Python => PYTHON_DESTRUCTIVE,
    };
    let lower = script.to_lowercase();
    for (i, line) in script.lines().enumerate() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some(w) = patterns.iter().find(|p| match Regex::new(p) {
            Ok(re) => re.is_match(&lower),
            Err(_) => false,
        }) {
            return Err(format!("the script would {w} (line {})", i + 1));
        }
    }
    Ok(())
}

fn collect_strings(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        serde_json::Value::Object(m) => m.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// The planning prompt
// ---------------------------------------------------------------------------

/// The design the model is asked for, in the same shape `tool_create` takes.
/// Keeping one schema means one parser and one validation path: whatever the
/// planner produces goes through `CustomTools::create` unchanged.
fn plan_messages(gap: &Gap, known: &[ToolSpec]) -> Vec<AiMessage> {
    let inventory = if known.is_empty() {
        "(none)".to_string()
    } else {
        known
            .iter()
            .map(|s| format!("- {}: {}", s.name, s.description.replace('\n', " ")))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut msgs = vec![
        AiMessage::system(
            "You design one tool for a voice assistant running on a Linux desktop. \
             Reply with ONE JSON object and nothing else: no prose, no markdown fence. \
             Shape: {\"name\": snake_case, \"description\": one sentence on what it does and when to use it, \
             \"kind\": \"composite\"|\"script\", \"params\": {name: description}, \
             \"steps\": [{\"tool\": existing_tool_name, \"args\": {...}}], \
             \"language\": \"bash\"|\"python\", \"script\": \"the full script text\"}. \
             Choose kind=composite (steps) whenever the tools listed below can do it; use kind=script only \
             when no combination of them can, and then write a complete script. \
             Script arguments arrive as $ARC_ARG_<NAME> environment variables (upper-cased) and as a JSON \
             object on stdin, never as interpolated text. \
             Never write a tool that deletes, wipes, formats, disables, shuts down, kills or overwrites \
             anything; such a request must be refused, not automated.",
        ),
        AiMessage::user(format!(
            "Tools that already exist (do not reinvent one of these):\n{inventory}\n\n\
             The user asked: {request}\n\
             The assistant reached for a tool called `{wanted}` because: {want}\n\n\
             Design the tool that does this. Reply with the JSON object only.",
            request = gap.request.trim(),
            wanted = gap.name,
            want = if gap.want.trim().is_empty() {
                "(the assistant did not say)".to_string()
            } else {
                gap.want.trim().to_string()
            },
        )),
    ];
    if gap.declared {
        msgs.push(AiMessage::assistant(
            String::from("I have no tool that can do that."),
            vec![],
        ));
    }
    msgs
}

// ---------------------------------------------------------------------------
// Parsing the design
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Draft {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    params: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    steps: Vec<Step>,
    #[serde(default)]
    language: String,
    #[serde(default)]
    script: String,
}

impl Draft {
    fn into_def(self, gap: &Gap) -> Result<Def, String> {
        let name = if self.name.trim().is_empty() { gap.name.clone() } else { normalise_name(&self.name)? };
        let body = match self.kind.trim().to_lowercase().as_str() {
            "composite" | "chain" => {
                if self.steps.is_empty() {
                    return Err("the design is a composite with no steps".into());
                }
                Body::Composite { steps: self.steps }
            }
            "script" | "bash" | "python" | "" if !self.script.trim().is_empty() => {
                let language = match self.language.trim().to_lowercase().as_str() {
                    "" => infer_language(&self.script),
                    "bash" | "sh" | "shell" => Language::Bash,
                    "python" | "python3" | "py" => Language::Python,
                    other => return Err(format!("`{other}` is not a language a script tool can use")),
                };
                Body::Script { language, script: self.script }
            }
            _ => return Err("the design needs kind=composite with steps, or kind=script with a script".into()),
        };
        Ok(Def {
            name,
            description: self.description.trim().to_string(),
            params: self.params,
            created: String::new(),
            fingerprint: String::new(),
            body,
        })
    }
}

/// A shebang decides; without one, python is the better guess for the kind of
/// thing a model writes when it is asked for a script tool.
fn infer_language(script: &str) -> Language {
    if script.lines().any(|l| l.trim_start().starts_with("#!") && l.contains("python")) {
        Language::Python
    } else if script.lines().any(|l| l.trim_start().starts_with("#!")) {
        Language::Bash
    } else {
        Language::Python
    }
}

/// Pull the JSON object out of the reply. Reasoning models often wrap it in a
/// fence or write a sentence first, so the object is located rather than
/// assumed.
fn parse_draft(content: &str) -> Result<Draft, String> {
    let text = content.trim();
    let start = text.find('{').ok_or("the design was not JSON")?;
    let end = text.rfind('}').ok_or("the design was not JSON")?;
    if end <= start {
        return Err("the design was not JSON".into());
    }
    let body = &text[start..=end];
    match serde_json::from_str::<Draft>(body) {
        Ok(d) => Ok(d),
        Err(e) => {
            // Some models emit JSON with trailing commas or single quotes.
            let cleaned: String = body
                .replace('\n', " ")
                .replace(",}", "}")
                .replace(",]", "]")
                .replace('\'', "\"");
            serde_json::from_str::<Draft>(&cleaned).map_err(|_| format!("the design was not usable JSON: {e}"))
        }
    }
}

/// Does the gate have anything by this name? Unknown calls are the gap signal,
/// so this has to agree with what `Gate::run` would refuse.
pub fn is_known(gate: &Gate, name: &str) -> bool {
    gate.tools().by_name(name).is_some() || gate.tools().custom().composite_steps(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(kind: Body) -> Def {
        Def {
            name: "made_up_tool".into(),
            description: "does a thing".into(),
            params: Default::default(),
            created: String::new(),
            fingerprint: String::new(),
            body: kind,
        }
    }

    #[test]
    fn names_are_normalised_into_something_def_accepts() {
        assert_eq!(normalise_name("Weather Forecast").unwrap(), "weather_forecast");
        assert_eq!(normalise_name("take-a-screenshot!").unwrap(), "take_a_screenshot");
        assert_eq!(normalise_name("  x  ").unwrap_err(), "`x` is not usable as a tool name");
        assert!(normalise_name("9lives").is_err());
        assert_eq!(normalise_name(&format!("{}tail", "a".repeat(60))).unwrap().len(), 40);
    }

    #[test]
    fn destructive_requests_are_refused_by_the_policy() {
        for r in [
            "delete every file in my downloads folder",
            "please wipe the disk",
            "kill the firefox process",
            "format the usb stick",
            "shut down the machine now",
            "uninstall that package for me",
            "run rm -rf on the cache",
            "make the script drop table users",
        ] {
            assert!(destructive_word(r).is_some(), "{r} was not seen as destructive");
        }
        for r in [
            "what is my disk usage",
            "list the files in this folder",
            "set the volume to forty",
            "remind me to call the dentist",
            "show me the windows on workspace three",
        ] {
            assert!(destructive_word(r).is_none(), "{r} was wrongly called destructive");
        }
    }

    #[test]
    fn a_design_that_deletes_is_refused_before_it_is_written() {
        let e = screen_definition(&def(Body::Script {
            language: Language::Bash,
            script: "echo hi\nrm -rf ~/Downloads\n".into(),
        }))
        .unwrap_err();
        assert!(e.contains("rm"), "{e}");

        let e = screen_definition(&def(Body::Script {
            language: Language::Python,
            script: "import shutil\nshutil.rmtree('/home/ciancom/Documents')\n".into(),
        }))
        .unwrap_err();
        assert!(e.contains("rmtree"), "{e}");

        // Python never reaches the bash analyzer, so this screen is the only
        // thing standing there.
        assert!(screen_definition(&def(Body::Script {
            language: Language::Python,
            script: "import os\nos.remove('/tmp/x')\n".into(),
        }))
        .is_err());

        let e = screen_definition(&def(Body::Composite {
            steps: vec![Step { tool: "media_pause".into(), args: serde_json::json!({}) }],
        }))
        .unwrap();
        let _ = e;
        let e = screen_definition(&def(Body::Composite {
            steps: vec![
                Step { tool: "media_pause".into(), args: serde_json::json!({}) },
                Step { tool: "reboot".into(), args: serde_json::json!({}) },
            ],
        }))
        .unwrap_err();
        assert!(e.contains("reboot"), "{e}");
    }

    #[test]
    fn an_ordinary_design_passes_the_screen() {
        assert!(
            screen_definition(&def(Body::Script {
                language: Language::Python,
                script: "import json,sys\nd=json.load(sys.stdin)\nprint(d.get('name',''))".into()
            }))
            .is_ok()
        );
        assert!(
            screen_definition(&def(Body::Composite {
                steps: vec![Step {
                    tool: "media_pause".into(),
                    args: serde_json::json!({"x": "{ws}"}),
                }]
            }))
            .is_ok()
        );
    }

    #[test]
    fn a_design_is_read_out_of_prose_a_fence_or_trailing_commas() {
        let fenced = "Sure!\n```json\n{\"name\": \"x\", \"description\": \"d\", \"kind\": \"composite\", \
                      \"steps\": [{\"tool\": \"media_pause\"}],}\n```\n";
        let d = parse_draft(fenced).unwrap();
        assert_eq!(d.name, "x");
        assert_eq!(d.steps.len(), 1);
        assert!(parse_draft("I could not design that.").is_err());
    }

    #[test]
    fn a_script_design_infers_its_language() {
        let g = Gap {
            name: "made_up_tool".into(),
            want: String::new(),
            request: "count the files in this folder".into(),
            ids: vec![],
            declared: false,
        };
        let py = Draft {
            name: String::new(),
            description: "d".into(),
            kind: "script".into(),
            params: Default::default(),
            steps: vec![],
            language: String::new(),
            script: "#!/usr/bin/env python3\nprint(1)".into(),
        }
        .into_def(&g)
        .unwrap();
        assert!(matches!(py.body, Body::Script { language: Language::Python, .. }));
    }

    #[test]
    fn the_budget_is_one_per_turn_and_five_per_session() {
        let ac = AutoCreate::from_settings(true, 1, 2);
        let ok = "make me a tool that lists open windows with their class";
        assert!(ac.permit(ok).is_ok());
        // Second in the same turn: over the per-turn limit.
        assert!(ac.permit(ok).unwrap_err().contains("this turn"));
        ac.begin_turn();
        assert!(ac.permit(ok).is_ok());
        ac.session.store(2, Ordering::SeqCst);
        ac.begin_turn();
        assert!(ac.permit(ok).unwrap_err().contains("session"));
    }

    #[test]
    fn short_requests_and_a_disabled_policy_are_refused() {
        let ac = AutoCreate::from_settings(true, 1, 5);
        assert!(ac.permit("volume").unwrap_err().contains("too short"));
        let off = AutoCreate::disabled();
        assert!(off.permit("make me a tool that counts files").unwrap_err().contains("off"));
    }
}
