//! Arc tool registry and built-in tools.
//!
//! Every tool implements [`Tool`], gets registered in [`Tools`], and can be
//! invoked by name from the router, CLI, daemon, or UI. Built-in tools wrap
//! arc-system and arc-hyprland so the rest of the codebase never calls those
//! crates directly.

use async_trait::async_trait;
use arc_config::{Config, ShellPolicy};
use arc_security::shell::ShellAnalyzer;
use arc_hyprland::dispatch::{Dispatch, WindowSel};
use arc_hyprland::Hyprland;
use arc_proto::RiskLevel;
use serde_json::{Value as Json, Map};
use std::collections::HashMap;
use std::sync::Arc;

/// Arguments passed to a tool at execution time. Always a JSON object.
pub type JsonMap = HashMap<String, Json>;

/// Outcome of a tool execution.
#[derive(Debug, Clone)]
pub enum ToolResult {
    Ok(Json),
    Error(String),
}

/// How risky one specific invocation is. Computed *before* running, from
/// the tool's base risk and (for tools like `shell_exec`) its arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct Assessment {
    pub risk: RiskLevel,
    /// `Some(reason)` = must never run, not even with confirmation.
    pub blocked: Option<String>,
    /// Ask for confirmation regardless of the global threshold.
    pub force_confirm: bool,
    /// Human-readable description shown in confirmation prompts.
    pub explanation: String,
}

impl Assessment {
    pub fn new(risk: RiskLevel, explanation: impl Into<String>) -> Self {
        Self { risk, blocked: None, force_confirm: false, explanation: explanation.into() }
    }
}

/// Model-facing description of a tool (OpenAI/Anthropic function schema).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Json,
}

fn no_params() -> Json {
    serde_json::json!({"type": "object", "properties": {}})
}

/// Every Arc tool implements this trait.
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON schema of the arguments object.
    fn parameters(&self) -> Json {
        no_params()
    }
    /// Risk of the tool when arguments don't change it.
    fn base_risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }
    /// Risk of this particular call. Never runs anything.
    fn assess(&self, _args: &JsonMap) -> Assessment {
        Assessment::new(self.base_risk(), self.description())
    }
    async fn execute(&self, args: &JsonMap) -> ToolResult;
    /// One short sentence describing a successful result, suitable for
    /// speaking aloud. `None` = let the caller describe it generically.
    fn summarize(&self, _result: &Json) -> Option<String> {
        None
    }
}

fn s(v: &Json, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(str::to_string)
}

fn volume_sentence(v: &Json) -> Option<String> {
    let p = v.get("percent")?.as_u64()?;
    Some(if v.get("muted").and_then(|m| m.as_bool()).unwrap_or(false) {
        format!("Muted. Volume is {p} percent.")
    } else {
        format!("Volume is {p} percent.")
    })
}

fn media_sentence(verb: &str, v: &Json) -> Option<String> {
    let title = s(v, "title");
    let artist = s(v, "artist");
    Some(match (title, artist) {
        (Some(t), Some(a)) => format!("{verb}: {t} by {a}."),
        (Some(t), None) => format!("{verb}: {t}."),
        _ => format!("{verb} on {}.", s(v, "player").unwrap_or_else(|| "the player".into())),
    })
}

/// Registry of all available tools.
pub struct Tools {
    by_name: HashMap<String, Arc<dyn Tool>>,
}

impl Default for Tools {
    fn default() -> Self {
        Self::new()
    }
}

impl Tools {
    /// Registry built from the default configuration.
    pub fn new() -> Self {
        Self::from_config(&Config::default()).expect("default config is valid")
    }

    /// Registry built from the user's configuration (shell policy,
    /// sensitive paths). Fails on invalid user regexes in the shell policy.
    pub fn from_config(cfg: &Config) -> Result<Self, String> {
        let sensitive: Vec<String> = cfg
            .files
            .sensitive_paths
            .iter()
            .map(|p| arc_config::paths::expand(p).to_string_lossy().into_owned())
            .collect();
        let analyzer = ShellAnalyzer::new(&cfg.permissions.shell, &sensitive)
            .map_err(|e| format!("invalid permissions.shell regex: {e}"))?;
        let mut t = Tools { by_name: HashMap::new() };
        t.register_builtins(ShellExec { analyzer, policy: cfg.permissions.shell.clone() });
        Ok(t)
    }

    /// Schemas for every registered tool, sorted by name.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut v: Vec<ToolSpec> = self
            .by_name
            .values()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.by_name.insert(tool.name().to_string(), tool);
    }

    pub fn by_name(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.by_name.get(name)
    }

    pub fn all_names(&self) -> Vec<String> {
        self.by_name.keys().cloned().collect()
    }

    fn register_builtins(&mut self, shell: ShellExec) {
        self.register(Arc::new(WorkspaceList));
        self.register(Arc::new(WorkspaceGoto));
        self.register(Arc::new(WindowList));
        self.register(Arc::new(WindowFocus));
        self.register(Arc::new(WindowMove));
        self.register(Arc::new(AudioVolumeSet));
        self.register(Arc::new(AudioVolumeMute));
        self.register(Arc::new(AudioVolumeUnmute));
        self.register(Arc::new(AudioVolumeGet));
        self.register(Arc::new(MediaPlay));
        self.register(Arc::new(MediaPause));
        self.register(Arc::new(MediaNext));
        self.register(Arc::new(MediaPrevious));
        self.register(Arc::new(MediaInfo));
        self.register(Arc::new(NotifySend));
        self.register(Arc::new(Lock));
        self.register(Arc::new(Sleep));
        self.register(Arc::new(Reboot));
        self.register(Arc::new(Shutdown));
        self.register(Arc::new(AppLaunch));
        self.register(Arc::new(NetworkStatus));
        self.register(Arc::new(PowerInfo));
        self.register(Arc::new(MonitorOverview));
        self.register(Arc::new(shell));
    }
}

fn try_hyprland() -> Option<Hyprland> {
    Hyprland::connect().ok()
}

// ---------------------------------------------------------------------------
// Hyprland tools
// ---------------------------------------------------------------------------

struct WorkspaceList;
#[async_trait]
impl Tool for WorkspaceList {
    fn name(&self) -> &str {
        "workspace_list"
    }
    fn description(&self) -> &str {
        "List all Hyprland workspaces"
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        let n = v.as_array()?.len();
        Some(format!("You have {n} workspace{} open.", if n == 1 { "" } else { "s" }))
    }
    async fn execute(&self, _args: &JsonMap) -> ToolResult {
        let h = match try_hyprland() {
            Some(h) => h,
            None => return ToolResult::Error("Hyprland not available".into()),
        };
        match h.workspaces().await {
            Ok(ws) => ToolResult::Ok(Json::Array(
                ws.into_iter().map(|w| {
                    let mut obj = Map::new();
                    obj.insert("id".into(), Json::Number((w.id).into()));
                    obj.insert("name".into(), Json::String(w.name));
                    obj.insert("monitor".into(), Json::String(w.monitor));
                    Json::Object(obj)
                }).collect(),
            )),
            Err(e) => ToolResult::Error(format!("failed to list workspaces: {e}")),
        }
    }
}

struct WorkspaceGoto;
#[async_trait]
impl Tool for WorkspaceGoto {
    fn name(&self) -> &str {
        "workspace_goto"
    }
    fn description(&self) -> &str {
        "Switch to a workspace by id or name"
    }
    fn parameters(&self) -> Json {
        serde_json::json!({"type": "object", "properties": {"id": {"type": "integer", "description": "Workspace number"}, "name": {"type": "string", "description": "Named workspace"}}, "required": []})
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        Some(format!("Switched to workspace {}.", s(v, "switched_to")?))
    }
    async fn execute(&self, args: &JsonMap) -> ToolResult {
        let id = args.get("id").and_then(|v| v.as_i64()).map(|i| i.to_string());
        let name = args.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
        let target = id.or(name).unwrap_or_default();
        if target.is_empty() {
            return ToolResult::Error("workspace_goto: need 'id' or 'name'".into());
        }
        let d = Dispatch::FocusWorkspace(target.clone());
        let h = match try_hyprland() {
            Some(h) => h,
            None => return ToolResult::Error("Hyprland not available".into()),
        };
        match h.dispatch(&d).await {
            Ok(_) => ToolResult::Ok(serde_json::json!({"switched_to": target})),
            Err(e) => ToolResult::Error(format!("failed to switch workspace: {e}")),
        }
    }
}

struct WindowList;
#[async_trait]
impl Tool for WindowList {
    fn name(&self) -> &str {
        "window_list"
    }
    fn description(&self) -> &str {
        "List all open windows"
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        let a = v.as_array()?;
        let mut classes: Vec<&str> = a.iter().filter_map(|w| w.get("class")?.as_str()).collect();
        classes.dedup();
        Some(match a.len() {
            0 => "No windows are open.".into(),
            n => format!("{n} window{} open: {}.", if n == 1 { "" } else { "s" }, classes.iter().take(6).copied().collect::<Vec<_>>().join(", ")),
        })
    }
    async fn execute(&self, _args: &JsonMap) -> ToolResult {
        let h = match try_hyprland() {
            Some(h) => h,
            None => return ToolResult::Error("Hyprland not available".into()),
        };
        match h.clients().await {
            Ok(clients) => ToolResult::Ok(Json::Array(
                clients.into_iter().map(|c| {
                    let mut obj = Map::new();
                    obj.insert("address".into(), Json::String(c.address));
                    obj.insert("class".into(), Json::String(c.class));
                    obj.insert("title".into(), Json::String(c.title));
                    obj.insert("workspace".into(), Json::String(c.workspace.name));
                    Json::Object(obj)
                }).collect(),
            )),
            Err(e) => ToolResult::Error(format!("failed to list windows: {e}")),
        }
    }
}

struct WindowFocus;
#[async_trait]
impl Tool for WindowFocus {
    fn name(&self) -> &str {
        "window_focus"
    }
    fn description(&self) -> &str {
        "Focus a window by address"
    }
    fn parameters(&self) -> Json {
        serde_json::json!({"type": "object", "properties": {"address": {"type": "string", "description": "Window address from window_list, e.g. 0x55d1..."}}, "required": ["address"]})
    }
    fn summarize(&self, _v: &Json) -> Option<String> {
        Some("Focused.".into())
    }
    async fn execute(&self, args: &JsonMap) -> ToolResult {
        let address = match args.get("address").and_then(|v| v.as_str()) {
            Some(a) => a.to_string(),
            None => return ToolResult::Error("window_focus: need 'address'".into()),
        };
        let d = Dispatch::FocusWindow(WindowSel::Address(address.clone()));
        let h = match try_hyprland() {
            Some(h) => h,
            None => return ToolResult::Error("Hyprland not available".into()),
        };
        match h.dispatch(&d).await {
            Ok(_) => ToolResult::Ok(serde_json::json!({"focused": address})),
            Err(e) => ToolResult::Error(format!("failed to focus window: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Audio tools
// ---------------------------------------------------------------------------

struct AudioVolumeSet;
#[async_trait]
impl Tool for AudioVolumeSet {
    fn name(&self) -> &str {
        "audio_volume_set"
    }
    fn description(&self) -> &str {
        "Set system volume to a percentage"
    }
    fn parameters(&self) -> Json {
        serde_json::json!({"type": "object", "properties": {"level": {"type": "integer", "minimum": 0, "maximum": 100}}, "required": ["level"]})
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        volume_sentence(v)
    }
    async fn execute(&self, args: &JsonMap) -> ToolResult {
        let level = args.get("level").and_then(|v| v.as_u64()).unwrap_or(50) as u32;
        match arc_system::audio::set(arc_system::audio::Channel::Output, level, 100).await {
            Ok(v) => ToolResult::Ok(serde_json::json!({"percent": v.percent, "muted": v.muted})),
            Err(e) => ToolResult::Error(format!("failed to set volume: {e}")),
        }
    }
}

struct AudioVolumeMute;
#[async_trait]
impl Tool for AudioVolumeMute {
    fn name(&self) -> &str {
        "audio_volume_mute"
    }
    fn description(&self) -> &str {
        "Mute the system audio"
    }
    fn summarize(&self, _v: &Json) -> Option<String> {
        Some("Muted.".into())
    }
    async fn execute(&self, _args: &JsonMap) -> ToolResult {
        match arc_system::audio::mute(arc_system::audio::Channel::Output, Some(true)).await {
            Ok(v) => ToolResult::Ok(serde_json::json!({"percent": v.percent, "muted": v.muted})),
            Err(e) => ToolResult::Error(format!("failed to mute: {e}")),
        }
    }
}

struct AudioVolumeUnmute;
#[async_trait]
impl Tool for AudioVolumeUnmute {
    fn name(&self) -> &str {
        "audio_volume_unmute"
    }
    fn description(&self) -> &str {
        "Unmute the system audio"
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        volume_sentence(v)
    }
    async fn execute(&self, _args: &JsonMap) -> ToolResult {
        match arc_system::audio::mute(arc_system::audio::Channel::Output, Some(false)).await {
            Ok(v) => ToolResult::Ok(serde_json::json!({"percent": v.percent, "muted": v.muted})),
            Err(e) => ToolResult::Error(format!("failed to unmute: {e}")),
        }
    }
}

struct AudioVolumeGet;
#[async_trait]
impl Tool for AudioVolumeGet {
    fn name(&self) -> &str {
        "audio_volume_get"
    }
    fn description(&self) -> &str {
        "Get current system volume"
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        volume_sentence(v)
    }
    async fn execute(&self, _args: &JsonMap) -> ToolResult {
        match arc_system::audio::get(arc_system::audio::Channel::Output).await {
            Ok(v) => ToolResult::Ok(serde_json::json!({"percent": v.percent, "muted": v.muted})),
            Err(e) => ToolResult::Error(format!("failed to get volume: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Media tools
// ---------------------------------------------------------------------------

struct MediaPlay;
#[async_trait]
impl Tool for MediaPlay {
    fn name(&self) -> &str {
        "media_play"
    }
    fn description(&self) -> &str {
        "Play the current media track"
    }
    fn parameters(&self) -> Json {
        serde_json::json!({"type": "object", "properties": {"name": {"type": "string", "description": "Player name (optional)"}}})
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        media_sentence("Playing", v)
    }
    async fn execute(&self, args: &JsonMap) -> ToolResult {
        let name = args.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
        let m = match arc_system::media::Media::connect().await {
            Ok(m) => m,
            Err(e) => return ToolResult::Error(format!("media player connection failed: {e}")),
        };
        match m.control(arc_system::media::MediaAction::Play, name.as_deref()).await {
            Ok(p) => ToolResult::Ok(serde_json::json!({
                "player": p.name, "status": p.status, "title": p.title, "artist": p.artist,
            })),
            Err(e) => ToolResult::Error(format!("failed to play: {e}")),
        }
    }
}

struct MediaPause;
#[async_trait]
impl Tool for MediaPause {
    fn name(&self) -> &str {
        "media_pause"
    }
    fn description(&self) -> &str {
        "Pause the current media track"
    }
    fn parameters(&self) -> Json {
        serde_json::json!({"type": "object", "properties": {"name": {"type": "string", "description": "Player name (optional)"}}})
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        media_sentence("Paused", v)
    }
    async fn execute(&self, args: &JsonMap) -> ToolResult {
        let name = args.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
        let m = match arc_system::media::Media::connect().await {
            Ok(m) => m,
            Err(e) => return ToolResult::Error(format!("media player connection failed: {e}")),
        };
        match m.control(arc_system::media::MediaAction::Pause, name.as_deref()).await {
            Ok(p) => ToolResult::Ok(serde_json::json!({
                "player": p.name, "status": p.status, "title": p.title, "artist": p.artist,
            })),
            Err(e) => ToolResult::Error(format!("failed to pause: {e}")),
        }
    }
}

struct MediaNext;
#[async_trait]
impl Tool for MediaNext {
    fn name(&self) -> &str {
        "media_next"
    }
    fn description(&self) -> &str {
        "Skip to the next track"
    }
    fn parameters(&self) -> Json {
        serde_json::json!({"type": "object", "properties": {"name": {"type": "string", "description": "Player name (optional)"}}})
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        media_sentence("Now playing", v)
    }
    async fn execute(&self, args: &JsonMap) -> ToolResult {
        let name = args.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
        let m = match arc_system::media::Media::connect().await {
            Ok(m) => m,
            Err(e) => return ToolResult::Error(format!("media player connection failed: {e}")),
        };
        match m.control(arc_system::media::MediaAction::Next, name.as_deref()).await {
            Ok(p) => ToolResult::Ok(serde_json::json!({
                "player": p.name, "status": p.status, "title": p.title, "artist": p.artist,
            })),
            Err(e) => ToolResult::Error(format!("failed to skip: {e}")),
        }
    }
}

struct MediaPrevious;
#[async_trait]
impl Tool for MediaPrevious {
    fn name(&self) -> &str {
        "media_previous"
    }
    fn description(&self) -> &str {
        "Go back to the previous track"
    }
    fn parameters(&self) -> Json {
        serde_json::json!({"type": "object", "properties": {"name": {"type": "string", "description": "Player name (optional)"}}})
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        media_sentence("Now playing", v)
    }
    async fn execute(&self, args: &JsonMap) -> ToolResult {
        let name = args.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
        let m = match arc_system::media::Media::connect().await {
            Ok(m) => m,
            Err(e) => return ToolResult::Error(format!("media player connection failed: {e}")),
        };
        match m.control(arc_system::media::MediaAction::Previous, name.as_deref()).await {
            Ok(p) => ToolResult::Ok(serde_json::json!({
                "player": p.name, "status": p.status, "title": p.title, "artist": p.artist,
            })),
            Err(e) => ToolResult::Error(format!("failed to go back: {e}")),
        }
    }
}

struct MediaInfo;
#[async_trait]
impl Tool for MediaInfo {
    fn name(&self) -> &str {
        "media_info"
    }
    fn description(&self) -> &str {
        "Get info about the currently playing track"
    }
    fn parameters(&self) -> Json {
        serde_json::json!({"type": "object", "properties": {"name": {"type": "string", "description": "Player name (optional)"}}})
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        media_sentence("Playing", v)
    }
    async fn execute(&self, args: &JsonMap) -> ToolResult {
        let name = args.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
        let m = match arc_system::media::Media::connect().await {
            Ok(m) => m,
            Err(e) => return ToolResult::Error(format!("media player connection failed: {e}")),
        };
        match m.pick(name.as_deref()).await {
            Ok(p) => ToolResult::Ok(serde_json::json!({
                "player": p.name, "status": p.status, "title": p.title,
                "artist": p.artist, "album": p.album,
            })),
            Err(e) => ToolResult::Error(format!("no media player found: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// System tools
// ---------------------------------------------------------------------------

struct NotifySend;
#[async_trait]
impl Tool for NotifySend {
    fn name(&self) -> &str {
        "notify_send"
    }
    fn description(&self) -> &str {
        "Send a desktop notification"
    }
    fn parameters(&self) -> Json {
        serde_json::json!({"type": "object", "properties": {"summary": {"type": "string"}, "body": {"type": "string"}, "urgency": {"type": "string", "enum": ["low", "normal", "critical"]}}, "required": ["body"]})
    }
    fn summarize(&self, _v: &Json) -> Option<String> {
        Some("Notification sent.".into())
    }
    async fn execute(&self, args: &JsonMap) -> ToolResult {
        let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("Arc");
        let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let urgency = match args.get("urgency").and_then(|v| v.as_str()) {
            Some("low") => arc_system::notify::Urgency::Low,
            Some("critical") => arc_system::notify::Urgency::Critical,
            _ => arc_system::notify::Urgency::Normal,
        };
        let timeout = args.get("timeout").and_then(|v| v.as_i64()).unwrap_or(5000) as i32;
        match arc_system::notify::send(summary, body, urgency, timeout).await {
            Ok(id) => ToolResult::Ok(serde_json::json!({"id": id})),
            Err(e) => ToolResult::Error(format!("failed to send notification: {e}")),
        }
    }
}

struct Lock;
#[async_trait]
impl Tool for Lock {
    fn name(&self) -> &str {
        "lock"
    }
    fn description(&self) -> &str {
        "Lock the screen"
    }
    fn summarize(&self, _v: &Json) -> Option<String> {
        Some("Locking the screen.".into())
    }
    async fn execute(&self, _args: &JsonMap) -> ToolResult {
        match arc_system::power::lock().await {
            Ok(msg) => ToolResult::Ok(serde_json::json!({"result": msg})),
            Err(e) => ToolResult::Error(format!("failed to lock: {e}")),
        }
    }
}

struct Sleep;
#[async_trait]
impl Tool for Sleep {
    fn name(&self) -> &str {
        "sleep"
    }
    fn description(&self) -> &str {
        "Put the device to sleep"
    }
    fn base_risk(&self) -> RiskLevel {
        RiskLevel::Caution
    }
    fn summarize(&self, _v: &Json) -> Option<String> {
        Some("Going to sleep.".into())
    }
    async fn execute(&self, _args: &JsonMap) -> ToolResult {
        match arc_system::power::sleep().await {
            Ok(msg) => ToolResult::Ok(serde_json::json!({"result": msg})),
            Err(e) => ToolResult::Error(format!("failed to sleep: {e}")),
        }
    }
}

struct Reboot;
#[async_trait]
impl Tool for Reboot {
    fn name(&self) -> &str {
        "reboot"
    }
    fn description(&self) -> &str {
        "Reboot the device"
    }
    fn base_risk(&self) -> RiskLevel {
        RiskLevel::Dangerous
    }
    fn summarize(&self, _v: &Json) -> Option<String> {
        Some("Rebooting.".into())
    }
    async fn execute(&self, _args: &JsonMap) -> ToolResult {
        match arc_system::power::reboot().await {
            Ok(msg) => ToolResult::Ok(serde_json::json!({"result": msg})),
            Err(e) => ToolResult::Error(format!("failed to reboot: {e}")),
        }
    }
}

struct Shutdown;
#[async_trait]
impl Tool for Shutdown {
    fn name(&self) -> &str {
        "shutdown"
    }
    fn description(&self) -> &str {
        "Shut down the device"
    }
    fn base_risk(&self) -> RiskLevel {
        RiskLevel::Dangerous
    }
    fn summarize(&self, _v: &Json) -> Option<String> {
        Some("Shutting down.".into())
    }
    async fn execute(&self, _args: &JsonMap) -> ToolResult {
        match arc_system::power::shutdown().await {
            Ok(msg) => ToolResult::Ok(serde_json::json!({"result": msg})),
            Err(e) => ToolResult::Error(format!("failed to shutdown: {e}")),
        }
    }
}

struct AppLaunch;
#[async_trait]
impl Tool for AppLaunch {
    fn name(&self) -> &str {
        "app_launch"
    }
    fn description(&self) -> &str {
        "Open an installed application by the name the user said (e.g. \"files\", \"VS Code\", \"terminal\", \"foot\"). \
         Resolves names against installed apps itself and focuses the app if it is already open, so pass the \
         user's words as-is and call it once. To open it on a specific workspace, pass `workspace`."
    }
    fn parameters(&self) -> Json {
        serde_json::json!({"type": "object", "properties": {
            "app": {"type": "string", "description": "App name as the user said it"},
            "workspace": {"type": "integer", "description": "Optional workspace number to switch to first"}
        }, "required": ["app"]})
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        s(v, "result").map(|r| format!("{}.", r.trim_end_matches('.')))
    }
    async fn execute(&self, args: &JsonMap) -> ToolResult {
        let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if app.is_empty() {
            return ToolResult::Error("app_launch: need 'app' argument".into());
        }
        if let Some(ws) = args.get("workspace").and_then(|v| v.as_i64()) {
            let Some(h) = try_hyprland() else { return ToolResult::Error("Hyprland not available".into()) };
            if let Err(e) = h.dispatch(&Dispatch::FocusWorkspace(ws.to_string())).await {
                return ToolResult::Error(format!("failed to switch to workspace {ws}: {e}"));
            }
        }
        match arc_system::apps::launch(&app).await {
            Ok(l) => ToolResult::Ok(serde_json::json!({"result": l.sentence()})),
            Err(e) => ToolResult::Error(e.to_string()),
        }
    }
}

struct WindowMove;
#[async_trait]
impl Tool for WindowMove {
    fn name(&self) -> &str {
        "window_move"
    }
    fn description(&self) -> &str {
        "Move a window to another workspace. Identify the window by app name/class (e.g. \"code\", \"firefox\") \
         or by address from window_list; omit both to move the focused window."
    }
    fn parameters(&self) -> Json {
        serde_json::json!({"type": "object", "properties": {
            "workspace": {"type": "integer", "description": "Target workspace number"},
            "app": {"type": "string", "description": "App name or window class, e.g. \"code\""},
            "address": {"type": "string", "description": "Window address from window_list"},
            "follow": {"type": "boolean", "description": "Also switch to that workspace (default false)"}
        }, "required": ["workspace"]})
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        Some(format!("Moved {} to workspace {}.", s(v, "window")?, v.get("workspace")?))
    }
    async fn execute(&self, args: &JsonMap) -> ToolResult {
        let Some(ws) = args.get("workspace").and_then(|v| v.as_i64()) else {
            return ToolResult::Error("window_move: need 'workspace'".into());
        };
        let follow = args.get("follow").and_then(|v| v.as_bool()).unwrap_or(false);
        let Some(h) = try_hyprland() else { return ToolResult::Error("Hyprland not available".into()) };
        let (sel, label) = if let Some(a) = args.get("address").and_then(|v| v.as_str()) {
            (WindowSel::Address(a.to_string()), a.to_string())
        } else if let Some(app) = args.get("app").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            let clients = match h.clients().await {
                Ok(c) => c,
                Err(e) => return ToolResult::Error(format!("failed to list windows: {e}")),
            };
            let want = app.to_lowercase().replace(' ', "");
            // Match class/title directly, or via the app's resolved desktop entry.
            let alt = match arc_system::apps::resolve(app) {
                Some(arc_system::apps::Resolved::Desktop(d)) => vec![d.exe.to_lowercase(), d.id.to_lowercase(), d.wm_class.to_lowercase()],
                Some(arc_system::apps::Resolved::Exe(e)) => vec![e.to_lowercase()],
                _ => vec![],
            };
            let hit = clients.iter().find(|c| {
                let class = c.class.to_lowercase();
                class == want || alt.iter().any(|a| !a.is_empty() && &class == a) || class.contains(&want)
                    || c.title.to_lowercase().replace(' ', "").contains(&want)
            });
            match hit {
                Some(c) => (WindowSel::Address(c.address.clone()), c.class.clone()),
                None => {
                    let open: Vec<&str> = clients.iter().map(|c| c.class.as_str()).collect();
                    return ToolResult::Error(format!(
                        "no open window matches \"{app}\". Open windows: {}. Don't guess; ask the user.",
                        open.join(", ")
                    ));
                }
            }
        } else {
            (WindowSel::Active, "the window".to_string())
        };
        let d = Dispatch::MoveToWorkspace { window: sel, workspace: ws.to_string(), follow };
        match h.dispatch(&d).await {
            Ok(_) => ToolResult::Ok(serde_json::json!({"window": label, "workspace": ws})),
            Err(e) => ToolResult::Error(format!("failed to move window: {e}")),
        }
    }
}

struct NetworkStatus;
#[async_trait]
impl Tool for NetworkStatus {
    fn name(&self) -> &str {
        "network_status"
    }
    fn description(&self) -> &str {
        "Get network interface status"
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        let conn = s(v, "connection").or_else(|| s(v, "name"))?;
        let online = v.get("reachable").and_then(|r| r.as_bool()).unwrap_or(true);
        Some(format!("Connected via {conn}{}.", if online { "" } else { ", but the internet isn't reachable" }))
    }
    async fn execute(&self, _args: &JsonMap) -> ToolResult {
        match arc_system::network::primary().await {
            Ok(iface) => ToolResult::Ok(serde_json::json!({
                "name": iface.name,
                "kind": iface.kind,
                "state": iface.state,
                "connection": iface.connection,
                "gateway": iface.gateway,
                "address": iface.address,
                "reachable": iface.reachable,
                "public_ip": iface.public_ip,
            })),
            Err(e) => ToolResult::Error(format!("failed to get network status: {e}")),
        }
    }
}

struct PowerInfo;
#[async_trait]
impl Tool for PowerInfo {
    fn name(&self) -> &str {
        "power_info"
    }
    fn description(&self) -> &str {
        "Get battery and power status"
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        s(v, "sentence")
    }
    async fn execute(&self, _args: &JsonMap) -> ToolResult {
        match arc_system::power::info().await {
            Ok(info) => ToolResult::Ok(serde_json::json!({
                "mode": info.mode,
                "battery_present": info.battery_present,
                "battery_percent": info.battery_percent,
                "time_left_minutes": info.time_left_minutes,
                "time_to_full_minutes": info.time_to_full_minutes,
                "ac_online": info.ac_online,
                "sentence": info.sentence(),
            })),
            Err(e) => ToolResult::Error(format!("failed to get power info: {e}")),
        }
    }
}

struct MonitorOverview;
#[async_trait]
impl Tool for MonitorOverview {
    fn name(&self) -> &str {
        "monitor_overview"
    }
    fn description(&self) -> &str {
        "Get system resource overview (CPU, memory, load)"
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        let cpu = v.get("cpu_usage")?.as_f64()?;
        let mem = v.get("memory_used_percent")?.as_f64()?;
        Some(format!("CPU at {cpu:.0} percent, memory {mem:.0} percent used."))
    }
    async fn execute(&self, _args: &JsonMap) -> ToolResult {
        match arc_system::monitor::overview().await {
            Ok(o) => ToolResult::Ok(serde_json::json!({
                "load_avg": o.load_avg,
                "cpu_usage": o.cpu_percent,
                "memory_used_gb": o.memory.used_bytes / 1_073_741_824,
                "memory_total_gb": o.memory.total_bytes / 1_073_741_824,
                "memory_used_percent": o.memory.used_percent,
            })),
            Err(e) => ToolResult::Error(format!("failed to get system overview: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Shell execution (guarded by security layer)
// ---------------------------------------------------------------------------

/// Runs a shell command. The command is classified by the security layer
/// first; `assess` exposes that so the executor can deny or ask before
/// `execute` is ever called. `execute` re-checks and refuses anything
/// blocked, so the tool is safe even if a caller skips the executor.
pub struct ShellExec {
    analyzer: ShellAnalyzer,
    policy: ShellPolicy,
}

impl ShellExec {
    fn command(args: &JsonMap) -> &str {
        args.get("command").and_then(|v| v.as_str()).unwrap_or("").trim()
    }
}

#[async_trait]
impl Tool for ShellExec {
    fn name(&self) -> &str {
        "shell_exec"
    }
    fn description(&self) -> &str {
        "Run a shell command (guarded by security layer)"
    }
    fn parameters(&self) -> Json {
        serde_json::json!({"type": "object", "properties": {"command": {"type": "string", "description": "Command line to run with sh -c"}}, "required": ["command"]})
    }
    fn base_risk(&self) -> RiskLevel {
        RiskLevel::Caution
    }
    fn assess(&self, args: &JsonMap) -> Assessment {
        let command = Self::command(args);
        if !self.policy.enabled {
            return Assessment {
                risk: RiskLevel::Dangerous,
                blocked: Some("shell commands are disabled (permissions.shell.enabled = false)".into()),
                force_confirm: false,
                explanation: "shell disabled".into(),
            };
        }
        if command.is_empty() {
            return Assessment {
                risk: RiskLevel::Safe,
                blocked: Some("shell_exec: need 'command' argument".into()),
                force_confirm: false,
                explanation: "empty command".into(),
            };
        }
        let v = self.analyzer.analyze(command);
        Assessment {
            risk: v.risk,
            force_confirm: v.force_confirm,
            explanation: format!("run `{command}` ({})", v.explanation()),
            blocked: v.blocked,
        }
    }
    fn summarize(&self, v: &Json) -> Option<String> {
        let out = s(v, "stdout").unwrap_or_default();
        Some(if out.is_empty() {
            "Done, no output.".into()
        } else if out.lines().count() == 1 && out.len() <= 120 {
            out
        } else {
            format!("Done. {} lines of output.", out.lines().count())
        })
    }
    async fn execute(&self, args: &JsonMap) -> ToolResult {
        let a = self.assess(args);
        if let Some(reason) = a.blocked {
            return ToolResult::Error(format!("shell_exec blocked: {reason}"));
        }
        let command = Self::command(args);
        let run = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .kill_on_drop(true)
            .output();
        let timeout = std::time::Duration::from_secs(self.policy.timeout_s.max(1));
        let output = match tokio::time::timeout(timeout, run).await {
            Ok(o) => o,
            Err(_) => return ToolResult::Error(format!("command timed out after {}s", timeout.as_secs())),
        };
        let max = self.policy.max_output_bytes;
        let clip = |b: &[u8]| {
            let s = String::from_utf8_lossy(&b[..b.len().min(max)]).trim().to_string();
            if b.len() > max { format!("{s}\n[output truncated]") } else { s }
        };
        match output {
            Ok(out) => {
                let stdout = clip(&out.stdout);
                let stderr = clip(&out.stderr);
                if out.status.success() {
                    ToolResult::Ok(serde_json::json!({"stdout": stdout, "stderr": stderr, "status": "ok"}))
                } else {
                    ToolResult::Error(format!(
                        "command exited with status {}: {}",
                        out.status,
                        if stderr.is_empty() { stdout } else { stderr }
                    ))
                }
            }
            Err(e) => ToolResult::Error(format!("failed to run command: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn args(kv: &[(&str, &str)]) -> JsonMap {
        kv.iter().map(|(k, v)| (k.to_string(), Json::String(v.to_string()))).collect()
    }

    #[test]
    fn registry_contains_all_expected_tools() {
        let t = Tools::new();
        let names = t.all_names();
        let expected = [
            "workspace_list",
            "workspace_goto",
            "window_list",
            "window_focus",
            "window_move",
            "audio_volume_set",
            "audio_volume_mute",
            "audio_volume_unmute",
            "audio_volume_get",
            "media_play",
            "media_pause",
            "media_next",
            "media_previous",
            "media_info",
            "notify_send",
            "lock",
            "sleep",
            "reboot",
            "shutdown",
            "app_launch",
            "network_status",
            "power_info",
            "monitor_overview",
            "shell_exec",
        ];
        for e in &expected {
            assert!(names.contains(&e.to_string()), "missing tool: {e}");
        }
    }

    #[tokio::test]
    async fn shell_exec_rejects_blocked_commands() {
        let t = Tools::new();
        let cmd = t.by_name("shell_exec").unwrap();
        let blocked = cmd
            .execute(&args(&[("command", "rm -rf /")]))
            .await;
        assert!(matches!(blocked, ToolResult::Error(e) if e.contains("blocked")));
    }

    #[tokio::test]
    async fn shell_exec_accepts_simple_commands() {
        let t = Tools::new();
        let cmd = t.by_name("shell_exec").unwrap();
        let result = cmd
            .execute(&args(&[("command", "echo hello")]))
            .await;
        assert!(matches!(result, ToolResult::Ok(_) | ToolResult::Error(_)));
    }

    #[test]
    fn power_tools_are_risky() {
        let t = Tools::new();
        let empty = JsonMap::new();
        assert_eq!(t.by_name("reboot").unwrap().assess(&empty).risk, RiskLevel::Dangerous);
        assert_eq!(t.by_name("shutdown").unwrap().assess(&empty).risk, RiskLevel::Dangerous);
        assert_eq!(t.by_name("lock").unwrap().assess(&empty).risk, RiskLevel::Safe);
    }

    #[test]
    fn shell_assess_uses_analyzer() {
        let t = Tools::new();
        let sh = t.by_name("shell_exec").unwrap();
        assert!(sh.assess(&args(&[("command", "rm -rf /")])).blocked.is_some());
        assert!(sh.assess(&args(&[("command", "ls")])).blocked.is_none());
    }

    #[test]
    fn shell_disabled_by_config() {
        let mut cfg = Config::default();
        cfg.permissions.shell.enabled = false;
        let t = Tools::from_config(&cfg).unwrap();
        assert!(t.by_name("shell_exec").unwrap().assess(&args(&[("command", "ls")])).blocked.is_some());
    }

    #[test]
    fn specs_have_object_schemas() {
        let specs = Tools::new().specs();
        assert_eq!(specs.len(), 24);
        assert!(specs.iter().all(|s| s.parameters["type"] == "object"));
    }

    #[test]
    fn summaries_are_sentences() {
        let t = Tools::new();
        let say = |tool: &str, v: Json| t.by_name(tool).unwrap().summarize(&v);
        assert_eq!(say("audio_volume_set", serde_json::json!({"percent": 40, "muted": false})).unwrap(), "Volume is 40 percent.");
        assert_eq!(
            say("media_next", serde_json::json!({"player": "spotify", "title": "Song", "artist": "Band"})).unwrap(),
            "Now playing: Song by Band."
        );
        assert_eq!(say("reboot", Json::Null).unwrap(), "Rebooting.");
        assert_eq!(say("shell_exec", serde_json::json!({"stdout": "hello"})).unwrap(), "hello");
    }

    #[tokio::test]
    async fn power_info_speaks_a_sentence() {
        let t = Tools::new();
        let tool = t.by_name("power_info").unwrap();
        let ToolResult::Ok(v) = tool.execute(&JsonMap::new()).await else { panic!() };
        let s = tool.summarize(&v).unwrap();
        assert!(s.ends_with('.') && !s.contains('{'), "{s}");
    }

    #[test]
    fn by_name_lookup() {
        let t = Tools::new();
        assert!(t.by_name("lock").is_some());
        assert!(t.by_name("nonexistent").is_none());
    }
}
