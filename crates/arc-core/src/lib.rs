//! Arc assistant core.
//!
//! * [`Router`]: deterministic fixed-command grammar ("lock", "set volume
//!   to 40"). Matches never involve the language model.
//! * [`gate::Gate`]: policy + confirmation around every tool call.
//! * [`agent::Agent`]: language-model tool loop for everything else.
//! * [`Assistant`]: one entry point that ties them together.

pub mod agent;
pub mod automations;
pub mod gate;

use agent::{Agent, AgentReply};
use automations::Automations;
use arc_ai::AiMessage;
use arc_config::Config;
use arc_memory::MemoryStore;
use arc_proto::{ActionOutcome, ActionRecord, PendingConfirmation, RiskLevel};
use arc_tools::{JsonMap, ToolResult, Tools};
use gate::{Gate, Outcome};
use regex::Regex;
use serde_json::{Value as Json, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Input to the router.
#[derive(Debug, Clone)]
pub struct NluInput {
    pub text: String,
    pub source: InputSource,
    pub context: HashMap<String, String>,
}

impl NluInput {
    pub fn text(text: impl Into<String>, source: InputSource) -> Self {
        Self { text: text.into(), source, context: HashMap::new() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    Text,
    Voice,
    Ui,
    Automation,
}

/// Output from the router.
#[derive(Debug, Clone)]
pub struct NluOutput {
    pub tool_name: Option<String>,
    pub args: JsonMap,
    pub confidence: f32,
    pub route: Route,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    FixedCommand,
    Ai,
    Unknown,
}

type Extract = Box<dyn Fn(&regex::Captures) -> JsonMap + Send + Sync>;
type Guard = Box<dyn Fn(&regex::Captures) -> bool + Send + Sync>;

/// A fixed-command grammar rule: regex → tool + optional param extraction.
struct FixedRule {
    pattern: Regex,
    tool: String,
    confidence: f32,
    extract: Option<Extract>,
    /// Extra check after the regex matches; the rule is skipped when false.
    guard: Option<Guard>,
}

impl FixedRule {
    fn new(pattern: &str, tool: &str, confidence: f32, extract: Option<Extract>) -> Result<Self, regex::Error> {
        Ok(FixedRule { pattern: Regex::new(pattern)?, tool: tool.to_string(), confidence, extract, guard: None })
    }

    fn when(mut self, g: impl Fn(&regex::Captures) -> bool + Send + Sync + 'static) -> Self {
        self.guard = Some(Box::new(g));
        self
    }
}

/// True if `prog` is an executable on PATH.
fn on_path(prog: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    if prog.is_empty() || prog.contains('/') {
        return false;
    }
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| {
            std::fs::metadata(d.join(prog)).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
        })
    })
}

/// Fixed-command router. Knows nothing about execution.
pub struct Router {
    fixed_rules: Vec<FixedRule>,
}

impl Router {
    pub fn new() -> Result<Self, regex::Error> {
        Ok(Router { fixed_rules: Self::build_fixed_rules()? })
    }

    fn build_fixed_rules() -> Result<Vec<FixedRule>, regex::Error> {
        Ok(vec![
            // Power
            FixedRule::new(r"(?i)^lock\s*(?:the\s*)?screen$", "lock", 1.0, None)?,
            FixedRule::new(r"(?i)^lock$", "lock", 0.95, None)?,
            FixedRule::new(r"(?i)^(?:go\s+to\s+)?sleep$", "sleep", 1.0, None)?,
            FixedRule::new(r"(?i)^suspend$", "sleep", 1.0, None)?,
            FixedRule::new(r"(?i)^reboot$", "reboot", 1.0, None)?,
            FixedRule::new(r"(?i)^restart$", "reboot", 0.95, None)?,
            FixedRule::new(r"(?i)^shutdown$", "shutdown", 1.0, None)?,
            FixedRule::new(r"(?i)^power\s*off$", "shutdown", 1.0, None)?,

            // Audio
            FixedRule::new(r"(?i)^mute$", "audio_volume_mute", 1.0, None)?,
            FixedRule::new(r"(?i)^unmute$", "audio_volume_unmute", 1.0, None)?,
            FixedRule::new(
                r"(?i)^set\s+volume\s+to\s+(\d+)",
                "audio_volume_set",
                1.0,
                Some(Box::new(|caps| {
                    let level: u32 = caps[1].parse().unwrap_or(50);
                    let mut m = HashMap::new();
                    m.insert("level".into(), serde_json::json!(level));
                    m
                })),
            )?,

            // Media
            FixedRule::new(r"(?i)^next\s*track$", "media_next", 1.0, None)?,
            FixedRule::new(r"(?i)^previous\s*track$", "media_previous", 1.0, None)?,
            FixedRule::new(r"(?i)^pause$", "media_pause", 1.0, None)?,
            FixedRule::new(r"(?i)^play$", "media_play", 1.0, None)?,
            FixedRule::new(
                r"(?i)^(?:what['\']s?\s+)?(?:now\s+)?play(?:ing)?$",
                "media_info",
                1.0,
                None,
            )?,

            // Workspaces
            FixedRule::new(r"(?i)^list\s+workspaces$", "workspace_list", 1.0, None)?,
            FixedRule::new(r"(?i)^show\s+workspaces$", "workspace_list", 1.0, None)?,
            FixedRule::new(
                r"(?i)^go\s+to\s+workspace\s+(\d+)",
                "workspace_goto",
                1.0,
                Some(Box::new(|caps| {
                    let id: i64 = caps[1].parse().unwrap_or(1);
                    let mut m = HashMap::new();
                    m.insert("id".into(), serde_json::json!(id));
                    m
                })),
            )?,
            FixedRule::new(
                r"(?i)^switch\s+to\s+workspace\s+(\d+)",
                "workspace_goto",
                1.0,
                Some(Box::new(|caps| {
                    let id: i64 = caps[1].parse().unwrap_or(1);
                    let mut m = HashMap::new();
                    m.insert("id".into(), serde_json::json!(id));
                    m
                })),
            )?,

            // Windows
            FixedRule::new(r"(?i)^list\s+windows$", "window_list", 1.0, None)?,
            FixedRule::new(r"(?i)^show\s+windows$", "window_list", 1.0, None)?,
            FixedRule::new(
                r"(?i)^focus\s+window\s+(\d+)",
                "window_focus",
                1.0,
                Some(Box::new(|caps| {
                    let addr = format!("0x{}", caps[1].parse::<u64>().unwrap_or(0));
                    let mut m = HashMap::new();
                    m.insert("address".into(), serde_json::json!(addr));
                    m
                })),
            )?,

            // Notifications
            FixedRule::new(
                r"(?i)^send\s+notification\s+(.+)",
                "notify_send",
                1.0,
                Some(Box::new(|caps| {
                    let mut m = HashMap::new();
                    m.insert("body".into(), serde_json::json!(caps[1].trim()));
                    m
                })),
            )?,

            // System info
            FixedRule::new(r"(?i)^network\s+status$", "network_status", 1.0, None)?,
            FixedRule::new(r"(?i)^internet\s+status$", "network_status", 0.95, None)?,
            FixedRule::new(r"(?i)^battery\s+status$", "power_info", 1.0, None)?,
            FixedRule::new(r"(?i)^power\s+status$", "power_info", 1.0, None)?,
            FixedRule::new(r"(?i)^system\s+overview$", "monitor_overview", 1.0, None)?,
            FixedRule::new(
                r"(?i)^what['\']s?\s+using\s+(?:my\s+)?memory$",
                "monitor_overview",
                0.95,
                None,
            )?,

            // Shell: only when the first word is a real program ("run ls -la",
            // "run htop"). "Run a speed test" / "run the shell command: …" is
            // natural language and goes to the model instead.
            FixedRule::new(
                r"(?i)^(?:run|execute)\s+([a-z0-9_./-]+(?:\s.*)?)$",
                "shell_exec",
                0.8,
                Some(Box::new(|caps| {
                    let mut m = HashMap::new();
                    m.insert("command".into(), serde_json::json!(caps[1].trim()));
                    m
                })),
            )?
            .when(|caps| {
                let first = caps[1].split_whitespace().next().unwrap_or("");
                !["a", "an", "the", "my", "this", "that", "some", "command", "shell"].contains(&first.to_lowercase().as_str())
                    && on_path(first)
            }),
        ])
    }

    /// Normalise an utterance: trim, drop trailing punctuation and a
    /// leading "please", so "Lock the screen, please." matches.
    fn normalise(text: &str) -> String {
        let t = text.trim().trim_end_matches(['.', '!', '?', ',']).trim();
        let t = t.strip_suffix(", please").or_else(|| t.strip_suffix(" please")).unwrap_or(t);
        let lower = t.to_lowercase();
        let t = if lower.starts_with("please ") { &t[7..] } else { t };
        t.trim().to_string()
    }

    /// Match against the fixed grammar; `Route::Ai` when nothing matches.
    pub fn route(&self, input: &NluInput) -> NluOutput {
        let text = Self::normalise(&input.text);
        for rule in &self.fixed_rules {
            if let Some(caps) = rule.pattern.captures(&text) {
                if rule.guard.as_ref().is_some_and(|g| !g(&caps)) {
                    continue;
                }
                let args = rule.extract.as_ref().map(|e| e(&caps)).unwrap_or_default();
                return NluOutput {
                    tool_name: Some(rule.tool.clone()),
                    args,
                    confidence: rule.confidence,
                    route: Route::FixedCommand,
                };
            }
        }
        NluOutput { tool_name: None, args: JsonMap::new(), confidence: 0.0, route: Route::Ai }
    }
}

// ---------------------------------------------------------------------------
// Assistant
// ---------------------------------------------------------------------------

/// What the assistant did with one request.
#[derive(Debug, Clone)]
pub struct Reply {
    pub text: String,
    pub route: Route,
    /// Every tool call made (successful, failed, denied or held).
    pub actions: Vec<ActionRecord>,
    /// Set when an action is waiting for the user's confirmation.
    pub pending: Option<PendingConfirmation>,
}

impl Reply {
    fn text_only(text: impl Into<String>, route: Route) -> Self {
        Self { text: text.into(), route, actions: vec![], pending: None }
    }

    /// Tools that actually ran successfully.
    pub fn succeeded(&self) -> Vec<&str> {
        self.actions.iter().filter(|a| a.outcome == ActionOutcome::Success).map(|a| a.tool.as_str()).collect()
    }
}

/// Short spoken confirmations/refusals for the latest pending action.
/// Only consulted while a confirmation is pending, so short words like "ok" are safe here.
fn yes_no(text: &str) -> Option<bool> {
    let t = Router::normalise(text).to_lowercase();
    let t = t.trim_end_matches(" please").trim_start_matches("yes ").trim();
    match t {
        "yes" | "yeah" | "yep" | "yup" | "sure" | "ok" | "okay" | "confirm" | "confirmed" | "do it" | "go ahead"
        | "yes do it" | "ok do it" | "okay do it" | "approve" | "approved" | "allow" | "allow it" | "allowed"
        | "you have my permission" | "you have permission" | "i give you permission" | "permission granted"
        | "granted" | "that's fine" | "thats fine" | "go for it" => Some(true),
        "no" | "nope" | "cancel" | "don't" | "dont" | "do not" | "stop" | "never mind" | "nevermind" | "deny"
        | "denied" | "reject" | "no thanks" | "don't do it" => Some(false),
        _ => None,
    }
}

fn summarise(tools: &Tools, tool: &str, result: &ToolResult) -> String {
    if let (ToolResult::Ok(v), Some(t)) = (result, tools.by_name(tool)) {
        if let Some(s) = t.summarize(v) {
            return s;
        }
    }
    match result {
        ToolResult::Ok(Json::Null) => format!("Done ({tool})."),
        ToolResult::Ok(v) => {
            let s = v.to_string();
            let s: String = s.chars().take(600).collect();
            format!("{tool}: {s}")
        }
        ToolResult::Error(e) => format!("{tool} failed: {e}"),
    }
}

pub struct Assistant {
    router: Router,
    gate: Arc<Gate>,
    agent: Option<Agent>,
    history: Mutex<Vec<AiMessage>>,
    max_history: usize,
    voice_confirm_dangerous: bool,
    provider_name: String,
    automations: Automations,
    store: Option<Arc<MemoryStore>>,
}

impl Assistant {
    /// Build from configuration. The language model is optional: with
    /// `ai.provider = "none"` (or a broken provider config) Arc still
    /// handles fixed commands.
    pub fn from_config(cfg: &Config, store: Option<Arc<MemoryStore>>) -> Result<Self, String> {
        let tools = Arc::new(Tools::from_config_with_memory(cfg, store.clone())?);
        let gate = Arc::new(Gate::new(tools, cfg));
        let (agent, provider_name) = match arc_ai::from_config(&cfg.ai) {
            Ok(p) => {
                let name = p.primary_name().to_string();
                (Some(Agent::new(p, gate.clone(), system_prompt(cfg, store.as_ref()), cfg.ai.max_tool_rounds)), name)
            }
            Err(e) => {
                tracing::info!(error = %e, "language model disabled");
                (None, "none".to_string())
            }
        };
        let mut a = Self::with_parts(gate, agent, store);
        a.voice_confirm_dangerous = cfg.voice.voice_confirm_dangerous;
        a.provider_name = provider_name;
        Ok(a)
    }

    /// Replace the user automations (loaded by the daemon at start / reload).
    /// Returns problems found (unknown tool names), for logging.
    pub fn set_automations(&mut self, file: arc_config::automations::AutomationFile) -> Vec<String> {
        self.automations = Automations::new(file);
        self.automations.problems(&self.gate)
    }

    pub fn automations(&self) -> &Automations {
        &self.automations
    }

    pub fn with_parts(gate: Arc<Gate>, agent: Option<Agent>, store: Option<Arc<MemoryStore>>) -> Self {
        Self {
            router: Router::new().expect("built-in grammar compiles"),
            gate,
            agent,
            history: Mutex::new(vec![]),
            max_history: 20,
            voice_confirm_dangerous: false,
            provider_name: "none".into(),
            automations: Automations::empty(),
            store,
        }
    }

    pub fn gate(&self) -> &Arc<Gate> {
        &self.gate
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    /// The memory store, if one is attached.
    pub fn memory(&self) -> Option<&Arc<MemoryStore>> {
        self.store.as_ref()
    }

    fn reply_from_outcome(&self, outcome: Outcome, route: Route) -> Reply {
        let record = outcome.record();
        let text = match &outcome {
            Outcome::Done { tool, result, .. } => summarise(self.gate.tools(), tool, result),
            Outcome::NeedsConfirmation(p) => format!("This will {}. Confirm?", p.explanation),
            Outcome::Denied { tool, reason, .. } => format!("I won't run {tool}: {reason}"),
        };
        let pending = match outcome {
            Outcome::NeedsConfirmation(p) => Some(p),
            _ => None,
        };
        Reply { text, route, actions: vec![record], pending }
    }

    /// Handle one request end to end.
    pub async fn handle(&self, input: &NluInput) -> Reply {
        // 1. A bare yes/no answers the most recent pending confirmation.
        if let (Some(answer), Some((id, risk))) = (yes_no(&input.text), self.gate.latest_pending_info()) {
            if !answer {
                self.gate.cancel(&id);
                return Reply::text_only("Cancelled.", Route::FixedCommand);
            }
            if input.source == InputSource::Voice && risk >= RiskLevel::Dangerous && !self.voice_confirm_dangerous {
                return Reply::text_only(
                    "That's a dangerous action, so I need you to confirm it in the Arc panel or with `arc confirm`.",
                    Route::FixedCommand,
                );
            }
            return self.confirm(&id).await;
        }

        // 2. User automations (exact phrase match; user phrases win over built-ins).
        if let Some(a) = self.automations.find(&input.text) {
            let r = self.automations.run(a, &self.gate).await;
            return Reply { text: r.text, route: Route::FixedCommand, actions: r.actions, pending: r.pending };
        }

        // 3. Deterministic grammar.
        let out = self.router.route(input);
        if let (Route::FixedCommand, Some(tool)) = (out.route, out.tool_name.as_deref()) {
            let ctx = json!({"source": input.source});
            return self.reply_from_outcome(self.gate.run(tool, out.args, ctx).await, Route::FixedCommand);
        }

        // 4. Language model.
        let Some(agent) = &self.agent else {
            return Reply::text_only("I don't know that command, and no language model is configured.", Route::Unknown);
        };
        let history = self.history.lock().unwrap().clone();
        match agent.ask(&history, &input.text).await {
            Ok(r) => {
                let (text, actions, pending) = match r {
                    AgentReply::Answer { text, actions } => (text, actions, None),
                    AgentReply::NeedsConfirmation { text, pending, actions } => (text, actions, Some(pending)),
                };
                let mut h = self.history.lock().unwrap();
                h.push(AiMessage::user(input.text.clone()));
                h.push(AiMessage::assistant(text.clone(), vec![]));
                let excess = h.len().saturating_sub(self.max_history);
                h.drain(..excess);
                let store = self.store.clone();
                if let Some(st) = &store {
                    let _ = st.append_turn(arc_memory::SessionTurnRole::User, input.text.clone());
                    let _ = st.append_turn(arc_memory::SessionTurnRole::Assistant, text.clone());
                }
                Reply { text, route: Route::Ai, actions, pending }
            }
            Err(e) => Reply::text_only(format!("The language model is unavailable: {e}"), Route::Ai),
        }
    }

    /// Invoke one tool directly (CLI `arc call`, automations). Still gated.
    pub async fn call_tool(&self, tool: &str, args: JsonMap) -> Reply {
        let outcome = self.gate.run(tool, args, json!({"source": "direct"})).await;
        self.reply_from_outcome(outcome, Route::FixedCommand)
    }

    /// Confirm a pending action by id (from the UI or `arc confirm`).
    pub async fn confirm(&self, id: &str) -> Reply {
        match self.gate.confirm(id).await {
            Ok(o) => self.reply_from_outcome(o, Route::FixedCommand),
            Err(e) => Reply::text_only(e.to_string(), Route::FixedCommand),
        }
    }

    /// Reject a pending action by id.
    pub fn reject(&self, id: &str) -> Reply {
        if self.gate.cancel(id) {
            Reply::text_only("Cancelled.", Route::FixedCommand)
        } else {
            Reply::text_only("There's no pending action with that id.", Route::FixedCommand)
        }
    }

    pub fn clear_history(&self) {
        self.history.lock().unwrap().clear();
    }
}

fn system_prompt(cfg: &Config, store: Option<&Arc<MemoryStore>>) -> String {
    use arc_config::PersonalityStyle::*;
    let style = match cfg.personality.style {
        Concise => "Answer in one or two short sentences. Occasional dry wit is fine.",
        Witty => "Answer briefly, with dry wit where it fits.",
        Formal => "Answer briefly and professionally. No jokes.",
    };
    let mut p = format!(
        "You are {name}, a voice assistant on the user's Linux desktop (Omarchy / Hyprland). {style} \
         Replies may be spoken aloud: no markdown, no lists unless asked. \
         Use the provided tools to act on the desktop; never claim you did something unless a tool \
         result confirms it. Some actions need the user's confirmation; when a tool result says so, \
         tell the user what you are waiting for. \
         Prefer the dedicated tools (app_launch, open_url, window_move, workspace_goto, window_focus) over shell_exec; \
         use open_url for websites and web searches. \
         If a tool fails, do not keep retrying variations or guessing other apps or commands: \
         at most one corrected retry, then briefly tell the user what went wrong. \
         Speech-to-text mishears words; if a request doesn't make sense, ask a short clarifying question \
         instead of acting. 'Desktop' means workspace.",
        name = cfg.general.name
    );
    if let Some(store) = store {
        let facts = store.prompt_block(8, None);
        let convo = store.recent_conversation();
        if !facts.is_empty() || !convo.is_empty() {
            p.push_str("\n\n");
            if !facts.is_empty() { p.push_str(&facts); }
            if !convo.is_empty() { p.push_str(&convo); }
        }
    }
    if !cfg.personality.custom_prompt.trim().is_empty() {
        p.push_str("\n\n");
        p.push_str(cfg.personality.custom_prompt.trim());
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router() -> Router {
        Router::new().unwrap()
    }

    #[test]
    fn lock_screen_matches() {
        let r = router();
        let out = r.route(&NluInput {
            text: "lock the screen".into(),
            source: InputSource::Voice,
            context: HashMap::new(),
        });
        assert_eq!(out.route, Route::FixedCommand);
        assert_eq!(out.tool_name.as_deref(), Some("lock"));
        assert_eq!(out.confidence, 1.0);
    }

    #[test]
    fn lock_short_matches() {
        let r = router();
        let out = r.route(&NluInput {
            text: "lock".into(),
            source: InputSource::Text,
            context: HashMap::new(),
        });
        assert_eq!(out.tool_name.as_deref(), Some("lock"));
        assert_eq!(out.confidence, 0.95);
    }

    #[test]
    fn sleep_matches() {
        let r = router();
        let out = r.route(&NluInput {
            text: "go to sleep".into(),
            source: InputSource::Voice,
            context: HashMap::new(),
        });
        assert_eq!(out.tool_name.as_deref(), Some("sleep"));
    }

    #[test]
    fn volume_extraction() {
        let r = router();
        let out = r.route(&NluInput {
            text: "set volume to 42".into(),
            source: InputSource::Text,
            context: HashMap::new(),
        });
        assert_eq!(out.tool_name.as_deref(), Some("audio_volume_set"));
        assert_eq!(out.args.get("level").and_then(|v| v.as_u64()), Some(42));
    }

    #[test]
    fn workspace_extraction() {
        let r = router();
        let out = r.route(&NluInput {
            text: "go to workspace 3".into(),
            source: InputSource::Text,
            context: HashMap::new(),
        });
        assert_eq!(out.tool_name.as_deref(), Some("workspace_goto"));
        assert_eq!(out.args.get("id").and_then(|v| v.as_i64()), Some(3));
    }

    #[test]
    fn notification_extraction() {
        let r = router();
        let out = r.route(&NluInput {
            text: "send notification hello world".into(),
            source: InputSource::Text,
            context: HashMap::new(),
        });
        assert_eq!(out.tool_name.as_deref(), Some("notify_send"));
        assert_eq!(out.args.get("body").and_then(|v| v.as_str()), Some("hello world"));
    }

    #[test]
    fn unknown_falls_to_ai() {
        let r = router();
        let out = r.route(&NluInput {
            text: "what is the meaning of life".into(),
            source: InputSource::Text,
            context: HashMap::new(),
        });
        assert_eq!(out.route, Route::Ai);
        assert!(out.tool_name.is_none());
    }

    #[test]
    fn shell_run_extraction() {
        let r = router();
        let out = r.route(&NluInput {
            text: "run ls -la".into(),
            source: InputSource::Text,
            context: HashMap::new(),
        });
        assert_eq!(out.tool_name.as_deref(), Some("shell_exec"));
        assert_eq!(out.args.get("command").and_then(|v| v.as_str()), Some("ls -la"));
    }

    #[test]
    fn normalisation() {
        let r = router();
        let out = r.route(&NluInput::text("Lock the screen, please.", InputSource::Voice));
        assert_eq!(out.tool_name.as_deref(), Some("lock"));
        let out = r.route(&NluInput::text("please mute!", InputSource::Voice));
        assert_eq!(out.tool_name.as_deref(), Some("audio_volume_mute"));
    }

    fn assistant() -> Assistant {
        let mut cfg = Config::default();
        cfg.ai.provider = arc_config::ProviderKind::None;
        Assistant::from_config(&cfg, None).unwrap()
    }

    #[tokio::test]
    async fn reboot_by_voice_is_held_then_cancelled() {
        let a = assistant();
        let r = a.handle(&NluInput::text("reboot", InputSource::Voice)).await;
        assert!(r.pending.is_some(), "reboot must wait for confirmation");
        assert!(r.succeeded().is_empty());
        let r = a.handle(&NluInput::text("no", InputSource::Voice)).await;
        assert_eq!(r.text, "Cancelled.");
        assert!(a.gate().latest_pending().is_none());
    }

    #[tokio::test]
    async fn safe_fixed_command_runs() {
        let a = assistant();
        let r = a.handle(&NluInput::text("battery status", InputSource::Text)).await;
        assert_eq!(r.route, Route::FixedCommand);
        assert!(r.pending.is_none());
        assert_eq!(r.succeeded(), vec!["power_info"]);
    }

    #[tokio::test]
    async fn blocked_shell_via_grammar_is_refused() {
        let a = assistant();
        let r = a.handle(&NluInput::text("run rm -rf /", InputSource::Voice)).await;
        assert!(r.pending.is_none());
        assert!(r.succeeded().is_empty());
        assert_eq!(r.actions[0].outcome, ActionOutcome::Denied);
        assert!(r.text.contains("won't run"));
    }

    #[test]
    fn run_only_routes_real_programs_to_the_shell() {
        let r = router();
        let route = |t: &str| r.route(&NluInput { text: t.into(), source: InputSource::Voice, context: HashMap::new() });
        let out = route("run ls -la");
        assert_eq!(out.tool_name.as_deref(), Some("shell_exec"));
        assert_eq!(out.args.get("command").and_then(|v| v.as_str()), Some("ls -la"));
        for natural in ["run the shell command: echo hi", "run a speed test", "run notarealprogram123 now", "execute my plan"] {
            assert_eq!(route(natural).route, Route::Ai, "{natural}");
        }
    }

    #[test]
    fn spoken_permission_phrases_are_understood() {
        for y in ["yes", "Yes, please.", "You have my permission.", "ok", "Go ahead", "allow it", "permission granted", "yes do it"] {
            assert_eq!(yes_no(y), Some(true), "{y}");
        }
        for n in ["no", "No thanks.", "cancel", "deny", "never mind"] {
            assert_eq!(yes_no(n), Some(false), "{n}");
        }
        for other in ["open firefox", "how can I give you permission", "okay arc open files"] {
            assert_eq!(yes_no(other), None, "{other}");
        }
    }

    #[tokio::test]
    async fn yes_without_pending_is_not_hijacked() {
        let a = assistant();
        let r = a.handle(&NluInput::text("yes", InputSource::Voice)).await;
        assert_eq!(r.route, Route::Unknown);
    }

    #[tokio::test]
    async fn no_model_configured_for_free_text() {
        let a = assistant();
        let r = a.handle(&NluInput::text("what's the meaning of life", InputSource::Text)).await;
        assert_eq!(r.route, Route::Unknown);
    }

    #[tokio::test]
    async fn voice_yes_cannot_confirm_dangerous_by_default() {
        let a = assistant();
        let r = a.handle(&NluInput::text("shutdown", InputSource::Voice)).await;
        let id = r.pending.expect("held").confirmation_id;
        let r = a.handle(&NluInput::text("yes", InputSource::Voice)).await;
        assert!(r.text.contains("Arc panel"), "{}", r.text);
        assert!(r.succeeded().is_empty());
        // Still pending; a typed/UI confirmation would be allowed.
        assert_eq!(a.gate().latest_pending().as_deref(), Some(id.as_str()));
        assert_eq!(a.reject(&id).text, "Cancelled.");
    }
}
