//! Arc wire protocol.
//!
//! Transport: newline-delimited JSON over a Unix domain socket at
//! `$XDG_RUNTIME_DIR/arc/arc.sock` (directory 0700, socket 0600). The daemon
//! additionally verifies the peer's UID with `SO_PEERCRED`, so only the owning
//! user can talk to it.
//!
//! Every client line is a [`ClientMessage`]; every daemon line is a
//! [`ServerMessage`]. Requests carry a client-chosen `id` that the daemon echoes
//! on the matching [`ServerMessage::Response`]. Connections that send
//! [`Request::Subscribe`] additionally receive [`Event`]s as they happen.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol version. Bumped on incompatible changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Socket file name inside the runtime directory.
pub const SOCKET_NAME: &str = "arc.sock";

/// Resolve the daemon socket path (`$ARC_SOCKET` overrides for tests).
pub fn socket_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("ARC_SOCKET") {
        return p.into();
    }
    runtime_dir().join(SOCKET_NAME)
}

/// `$XDG_RUNTIME_DIR/arc` (falls back to `/run/user/<uid>/arc`, then /tmp).
pub fn runtime_dir() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("ARC_RUNTIME_DIR") {
        return p.into();
    }
    let base = std::env::var("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| {
        // SAFETY-free fallback: read the uid from /proc/self (no libc here).
        let uid = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("Uid:"))
                    .and_then(|l| l.split_whitespace().nth(1).map(str::to_string))
            })
            .unwrap_or_else(|| "0".into());
        let p = std::path::PathBuf::from(format!("/run/user/{uid}"));
        if p.is_dir() { p } else { std::env::temp_dir() }
    });
    base.join("arc")
}

// ---------------------------------------------------------------------------
// Envelopes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMessage {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMessage {
    Response {
        id: u64,
        #[serde(flatten)]
        result: ResponseBody,
    },
    Event {
        #[serde(flatten)]
        event: Event,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseBody {
    Ok { data: Value },
    Error { error: ErrorInfo },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorInfo {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    NotFound,
    Denied,
    Unavailable,
    Failed,
    Internal,
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Health check. Returns `{"pong": true, "version": ...}`.
    Ping,
    /// Identify the client (optional; used for logging and voice routing).
    Hello { client: ClientKind, version: String },
    /// Subscribe this connection to daemon events.
    Subscribe { topics: Vec<Topic> },
    /// Natural-language request. Returns [`AskResult`].
    Ask {
        text: String,
        #[serde(default)]
        source: InputSource,
    },
    /// Approve or reject a pending confirmation. Returns [`AskResult`].
    Confirm { confirmation_id: String, approve: bool },
    /// Invoke a tool directly (debug / automation). Still passes the
    /// permission layer; returns [`AskResult`].
    CallTool {
        tool: String,
        #[serde(default)]
        args: Value,
    },
    /// Full status report. Returns [`StatusReport`].
    Status,
    /// Compact status for status bars. Returns [`BarStatus`].
    BarStatus,
    /// List registered tools. Returns `Vec<ToolInfo>`.
    Tools,
    /// Change one tool's safety classification from the UI.
    ///
    /// `level = null` clears the override and restores the tool's built-in
    /// risk. Returns the updated [`ToolInfo`] for that tool.
    SetToolClass {
        tool: String,
        /// `safe` | `caution` | `dangerous`, or null to reset.
        level: Option<RiskLevel>,
    },
    /// Current desktop snapshot. Returns the desktop state JSON.
    Desktop,
    /// Memory operations.
    Memory(MemoryRequest),
    /// Automation (alias/routine) operations.
    Automations(AutomationRequest),
    /// Permission policy inspection.
    Permissions,
    /// Recent audit log entries.
    Audit { limit: usize },
    /// Voice pipeline control (forwarded to the voice service).
    Voice { command: VoiceCommand },
    /// Voice service → daemon report.
    VoiceReport { report: VoiceReport },
    /// Reload configuration from disk.
    Reload,
    /// Clear the short-term conversation context.
    ResetConversation,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    #[default]
    Text,
    Voice,
    Ui,
    Automation,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Cli,
    Ui,
    Bar,
    Voice,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    /// Assistant state changes, replies, tool activity.
    Assistant,
    /// Desktop state changes (workspace/window focus).
    Desktop,
    /// Voice commands destined for the voice service.
    VoiceControl,
    /// Audio levels / transcripts from the voice service (for the UI).
    VoiceActivity,
    /// Log-worthy errors.
    Errors,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum MemoryRequest {
    List { category: Option<String> },
    Remember { category: String, key: String, value: String },
    Forget { key: String },
    ForgetLast,
    Clear,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AutomationRequest {
    List,
    Run { name: String },
}

// ---------------------------------------------------------------------------
// Voice
// ---------------------------------------------------------------------------

/// Commands the daemon (or a client, via the daemon) sends to the voice service.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum VoiceCommand {
    /// Begin capturing an utterance immediately (push-to-talk press / UI button).
    StartListening,
    /// End the current capture and transcribe it (push-to-talk release).
    StopListening,
    /// Toggle a capture (single-key push-to-talk).
    ToggleListening,
    /// Abort capture without transcribing.
    CancelListening,
    /// Change activation mode at runtime.
    SetMode { mode: VoiceMode },
    /// Speak text.
    Speak {
        text: String,
        utterance_id: String,
        /// Start listening (no wake word) once this finishes playing, e.g.
        /// after Arc asks a question or for confirmation.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        listen_after: bool,
    },
    /// Stop any speech in progress.
    StopSpeaking,
    /// Re-read configuration.
    Reload,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum VoiceMode {
    /// Only push-to-talk / explicit start.
    #[default]
    PushToTalk,
    /// Wake word opens a capture.
    WakeWord,
    /// Every detected speech segment is treated as a command (needs the wake
    /// word as a prefix unless `require_wake_word_in_continuous = false`).
    Continuous,
}

/// Reports the voice service sends to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "report", rename_all = "snake_case")]
pub enum VoiceReport {
    /// Component readiness (sent on start and whenever it changes).
    Health {
        mic: ComponentHealth,
        vad: ComponentHealth,
        wake: ComponentHealth,
        stt: ComponentHealth,
        tts: ComponentHealth,
        mode: VoiceMode,
        input_device: String,
        output_device: String,
    },
    /// Wake word detected.
    WakeDetected {
        keyword: String,
    },
    /// Capture started (via wake word, PTT, or VAD in continuous mode).
    ListeningStarted,
    /// Capture ended; transcription running.
    Transcribing,
    /// Final transcript of one utterance.
    Transcript {
        text: String,
        stt_ms: u64,
        audio_ms: u64,
    },
    /// Capture ended without usable speech.
    NoSpeech,
    /// Coarse input level, 0..1 (throttled; only while listening).
    Level {
        rms: f32,
    },
    SpeakingStarted {
        utterance_id: String,
    },
    SpeakingFinished {
        utterance_id: String,
        interrupted: bool,
    },
    Error {
        component: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ComponentHealth {
    pub status: HealthStatus,
    #[serde(default)]
    pub detail: String,
}

impl ComponentHealth {
    pub fn ok(detail: impl Into<String>) -> Self {
        Self { status: HealthStatus::Ok, detail: detail.into() }
    }
    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self { status: HealthStatus::Unavailable, detail: detail.into() }
    }
    pub fn disabled() -> Self {
        Self { status: HealthStatus::Disabled, detail: String::new() }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Ok,
    Degraded,
    Unavailable,
    Disabled,
    Unknown,
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// Risk classification of an action. Ordered: Safe < Caution < Dangerous.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Safe,
    Caution,
    Dangerous,
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RiskLevel::Safe => "safe",
            RiskLevel::Caution => "caution",
            RiskLevel::Dangerous => "dangerous",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskResult {
    /// What Arc says back (also spoken for voice requests).
    pub reply: String,
    /// Actions taken (or attempted) while handling the request.
    pub actions: Vec<ActionRecord>,
    /// Set when execution is paused awaiting confirmation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingConfirmation>,
    /// How the request was understood: "rule", "llm", "memory", "automation".
    pub route: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionRecord {
    pub tool: String,
    pub args: Value,
    pub risk: RiskLevel,
    pub outcome: ActionOutcome,
    /// Short human summary of the result or failure.
    pub summary: String,
    /// Why this call carried a warning — set when the user has classified the
    /// tool `caution`, so the UI and the CLI can say so out loud. Nothing
    /// else populates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    Success,
    Failed,
    Denied,
    AwaitingConfirmation,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingConfirmation {
    pub confirmation_id: String,
    pub tool: String,
    pub args: Value,
    pub risk: RiskLevel,
    /// Plain-language explanation of exactly what will happen.
    pub explanation: String,
    /// Seconds until the confirmation expires.
    pub expires_in_s: u64,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    /// The risk the runtime will actually apply: the user's classification if
    /// they set one, otherwise the tool's own assessed level.
    pub risk: RiskLevel,
    /// The level the tool declares in code. Shown next to `risk` so the UI
    /// can mark a row as overridden.
    pub default_risk: RiskLevel,
    /// True when the user has classified this tool differently from its
    /// built-in level.
    pub reclassified: bool,
    pub category: String,
    pub parameters: Value,
    pub enabled: bool,
    /// `composite` or `script` for a tool Arc made for itself; absent for
    /// built-ins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub made_by_arc: Option<String>,
    /// False for tools that are dangerous by nature and can only be raised
    /// (reboot, shutdown, code). The picker shows those as locked.
    #[serde(default = "yes")]
    pub lowerable: bool,
    /// Why the tool is disabled/unavailable, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssistantState {
    #[default]
    Idle,
    Listening,
    Thinking,
    Executing,
    Speaking,
    Error,
}

impl AssistantState {
    pub fn label(self) -> &'static str {
        match self {
            AssistantState::Idle => "Idle",
            AssistantState::Listening => "Listening",
            AssistantState::Thinking => "Thinking",
            AssistantState::Executing => "Executing",
            AssistantState::Speaking => "Speaking",
            AssistantState::Error => "Error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ComponentStatus {
    pub name: String,
    pub status: HealthStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusReport {
    pub version: String,
    pub pid: u32,
    pub uptime_s: u64,
    pub state: AssistantState,
    pub components: Vec<ComponentStatus>,
    pub ai_provider: String,
    pub ai_model: String,
    pub voice_mode: Option<VoiceMode>,
    pub tools_enabled: usize,
    pub tools_total: usize,
    pub active_workspace: Option<String>,
    pub active_window: Option<String>,
    pub cpu_percent: Option<f32>,
    pub mem_percent: Option<f32>,
    pub gpu_percent: Option<f32>,
    pub mic_muted: Option<bool>,
    pub last_command: Option<String>,
    pub last_reply: Option<String>,
    pub last_action: Option<String>,
    pub recent_errors: Vec<String>,
    pub rss_kb: Option<u64>,
}

/// Compact, cheap status for bar widgets (Omarchy bar / Waybar).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BarStatus {
    pub state: AssistantState,
    pub text: String,
    pub tooltip: String,
    pub class: String,
    pub mic_muted: Option<bool>,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    State {
        state: AssistantState,
        detail: String,
    },
    /// Complete bar status, pushed whenever it changes (state, voice health,
    /// last command). Bar clients render this directly and never need to
    /// query back, so fast state changes can't be missed or misreported.
    Bar {
        status: BarStatus,
    },
    /// User input as understood (transcript or typed text).
    Heard {
        text: String,
        source: InputSource,
    },
    /// One step of the model's thinking during a turn: its reasoning text (when
    /// the provider exposes it) and any words it wrote alongside tool calls.
    /// For display only -- never spoken, never fed back to the model.
    Thought {
        round: u32,
        reasoning: String,
        text: String,
    },
    /// What Hermes is doing right now, while a `code` task runs. Display
    /// only -- never spoken automatically and never fed back to the model.
    CodeProgress {
        tool: String,
        detail: String,
        step: u32,
        elapsed_s: u64,
    },
    ToolStarted {
        tool: String,
        args: Value,
        risk: RiskLevel,
    },
    ToolFinished {
        record: ActionRecord,
    },
    ConfirmationRequired {
        pending: PendingConfirmation,
    },
    Reply {
        text: String,
        route: String,
    },
    Desktop {
        active_workspace: String,
        active_window: String,
    },
    VoiceControl {
        command: VoiceCommand,
    },
    VoiceLevel {
        rms: f32,
    },
    Error {
        component: String,
        message: String,
    },
}

impl Event {
    pub fn topic(&self) -> Topic {
        match self {
            Event::State { .. }
            | Event::Bar { .. }
            | Event::Heard { .. }
            | Event::Thought { .. }
            | Event::CodeProgress { .. }
            | Event::ToolStarted { .. }
            | Event::ToolFinished { .. }
            | Event::ConfirmationRequired { .. }
            | Event::Reply { .. } => Topic::Assistant,
            Event::Desktop { .. } => Topic::Desktop,
            Event::VoiceControl { .. } => Topic::VoiceControl,
            Event::VoiceLevel { .. } => Topic::VoiceActivity,
            Event::Error { .. } => Topic::Errors,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

impl ServerMessage {
    pub fn ok(id: u64, data: impl Serialize) -> Self {
        ServerMessage::Response {
            id,
            result: ResponseBody::Ok { data: serde_json::to_value(data).unwrap_or(Value::Null) },
        }
    }
    pub fn err(id: u64, code: ErrorCode, message: impl Into<String>) -> Self {
        ServerMessage::Response {
            id,
            result: ResponseBody::Error { error: ErrorInfo { code, message: message.into() } },
        }
    }
}

/// Encode a message as a single JSON line (with trailing newline).
pub fn encode_line<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).expect("protocol types always serialize");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn client_message_roundtrip() {
        let m = ClientMessage {
            id: 7,
            request: Request::Ask { text: "open firefox".into(), source: InputSource::Voice },
        };
        let line = encode_line(&m);
        assert!(line.ends_with('\n'));
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["type"], "ask");
        assert_eq!(v["source"], "voice");
        let back: ClientMessage = serde_json::from_str(&line).unwrap();
        assert!(matches!(back.request, Request::Ask { ref text, .. } if text == "open firefox"));
    }

    #[test]
    fn ask_source_defaults_to_text() {
        let m: ClientMessage = serde_json::from_value(json!({"id":1,"type":"ask","text":"hi"})).unwrap();
        assert!(matches!(m.request, Request::Ask { source: InputSource::Text, .. }));
    }

    #[test]
    fn unit_requests_parse() {
        for t in ["ping", "status", "bar_status", "tools", "desktop", "permissions", "reload"] {
            let m: Result<ClientMessage, _> = serde_json::from_value(json!({"id":1,"type":t}));
            assert!(m.is_ok(), "{t} failed: {m:?}");
        }
    }

    /// The UI sends this when the user picks a level in the tool list. A null
    /// level means "reset to the tool's own risk", which is a different
    /// request from `"safe"` and must not decode as it.
    #[test]
    fn set_tool_class_roundtrip() {
        let m: ClientMessage =
            serde_json::from_value(json!({"id":4,"type":"set_tool_class","tool":"reboot","level":"safe"}))
                .unwrap();
        assert!(matches!(m.request, Request::SetToolClass { ref tool, level: Some(RiskLevel::Safe) }
            if tool == "reboot"));
        let m: ClientMessage =
            serde_json::from_value(json!({"id":5,"type":"set_tool_class","tool":"reboot","level":null}))
                .unwrap();
        assert!(matches!(m.request, Request::SetToolClass { level: None, .. }));
        // A level the UI should never send is a parse error, not a default.
        assert!(
            serde_json::from_value::<ClientMessage>(
                json!({"id":6,"type":"set_tool_class","tool":"reboot","level":"spicy"})
            )
            .is_err()
        );
    }

    #[test]
    fn nested_requests_parse() {
        let m: ClientMessage = serde_json::from_value(
            json!({"id":2,"type":"memory","op":"remember","category":"preference","key":"terminal","value":"kitty"}),
        )
        .unwrap();
        assert!(matches!(m.request, Request::Memory(MemoryRequest::Remember { .. })));
        let m: ClientMessage =
            serde_json::from_value(json!({"id":3,"type":"voice","command":{"action":"toggle_listening"}}))
                .unwrap();
        assert!(matches!(m.request, Request::Voice { command: VoiceCommand::ToggleListening }));
    }

    #[test]
    fn server_messages_are_tagged() {
        let ok = serde_json::to_value(ServerMessage::ok(3, json!({"a":1}))).unwrap();
        assert_eq!(ok, json!({"kind":"response","id":3,"status":"ok","data":{"a":1}}));
        let err = serde_json::to_value(ServerMessage::err(4, ErrorCode::Denied, "no")).unwrap();
        assert_eq!(err["status"], "error");
        assert_eq!(err["error"]["code"], "denied");
        let ev = serde_json::to_value(ServerMessage::Event {
            event: Event::State { state: AssistantState::Thinking, detail: String::new() },
        })
        .unwrap();
        assert_eq!(ev["kind"], "event");
        assert_eq!(ev["event"], "state");
        assert_eq!(ev["state"], "thinking");
    }

    #[test]
    fn risk_ordering() {
        assert!(RiskLevel::Safe < RiskLevel::Caution);
        assert!(RiskLevel::Caution < RiskLevel::Dangerous);
    }

    #[test]
    fn voice_report_roundtrip() {
        let r = VoiceReport::Transcript { text: "open firefox".into(), stt_ms: 80, audio_ms: 1900 };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["report"], "transcript");
        let back: VoiceReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn event_topics() {
        assert_eq!(Event::VoiceLevel { rms: 0.1 }.topic(), Topic::VoiceActivity);
        assert_eq!(Event::VoiceControl { command: VoiceCommand::StopSpeaking }.topic(), Topic::VoiceControl);
    }
}
