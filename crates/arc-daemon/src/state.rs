//! Shared daemon state: the assistant, current state machine, event bus,
//! status reporting and the bar status file.

use arc_config::Config;
use arc_core::{Assistant, InputSource as CoreSource, NluInput, Reply, Route};
use arc_proto::{
    AskResult, AssistantState, BarStatus, ComponentStatus, Event, HealthStatus, InputSource, StatusReport, VoiceCommand,
    VoiceMode,
};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::broadcast;

#[derive(Debug, Default, Clone)]
pub struct Mutable {
    pub state: AssistantState,
    pub last_command: Option<String>,
    pub last_reply: Option<String>,
    pub last_action: Option<String>,
    pub recent_errors: Vec<String>,
    pub voice: Option<VoiceStatus>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VoiceStatus {
    pub mode: VoiceMode,
    pub components: Vec<ComponentStatus>,
    pub running: bool,
}

pub struct Daemon {
    pub assistant: Assistant,
    pub config: Config,
    pub events: broadcast::Sender<Event>,
    pub started: Instant,
    pub bar_file: Option<PathBuf>,
    m: Mutex<Mutable>,
    utterance: AtomicU64,
}

fn to_core(s: InputSource) -> CoreSource {
    match s {
        InputSource::Text => CoreSource::Text,
        InputSource::Voice => CoreSource::Voice,
        InputSource::Ui => CoreSource::Ui,
        InputSource::Automation => CoreSource::Automation,
    }
}

fn route_name(r: Route) -> &'static str {
    match r {
        Route::FixedCommand => "rule",
        Route::Ai => "llm",
        Route::Unknown => "unknown",
    }
}

impl Daemon {
    pub fn new(config: Config, bar_file: Option<PathBuf>) -> Result<Self, String> {
        let mut assistant = Assistant::from_config(&config)?;
        let path = std::env::var_os("ARC_AUTOMATIONS")
            .map(PathBuf::from)
            .unwrap_or_else(arc_config::paths::automations_file);
        match arc_config::automations::load(&path) {
            Ok(file) => {
                let n = file.automations.len();
                for p in assistant.set_automations(file) {
                    tracing::warn!("{p}");
                }
                tracing::info!(count = n, path = %path.display(), "automations loaded");
            }
            // A broken automations file must not stop Arc from starting.
            Err(e) => tracing::error!(error = %e, "automations not loaded"),
        }
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            assistant,
            config,
            events,
            started: Instant::now(),
            bar_file,
            m: Mutex::new(Mutable::default()),
            utterance: AtomicU64::new(0),
        })
    }

    pub fn emit(&self, e: Event) {
        // No subscribers is fine.
        let _ = self.events.send(e);
    }

    pub fn snapshot(&self) -> Mutable {
        self.m.lock().unwrap().clone()
    }

    pub fn set_state(&self, state: AssistantState, detail: &str) {
        {
            let mut m = self.m.lock().unwrap();
            if m.state == state {
                return;
            }
            m.state = state;
        }
        self.emit(Event::State { state, detail: detail.into() });
        self.publish_bar();
    }

    pub fn record_error(&self, component: &str, message: &str) {
        {
            let mut m = self.m.lock().unwrap();
            m.recent_errors.push(format!("{component}: {message}"));
            let n = m.recent_errors.len();
            if n > 10 {
                m.recent_errors.drain(..n - 10);
            }
        }
        self.emit(Event::Error { component: component.into(), message: message.into() });
    }

    pub fn set_voice(&self, v: Option<VoiceStatus>) {
        {
            let mut m = self.m.lock().unwrap();
            if m.voice == v {
                return;
            }
            m.voice = v;
        }
        self.publish_bar();
    }

    /// Push the bar status to subscribers and the bar file.
    pub fn publish_bar(&self) {
        let status = self.bar_status();
        self.write_bar_status(&status);
        self.emit(Event::Bar { status });
    }

    pub fn next_utterance_id(&self) -> String {
        format!("u{}", self.utterance.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Handle one natural-language request end to end, with events.
    pub async fn ask(&self, text: &str, source: InputSource) -> AskResult {
        let start = Instant::now();
        self.emit(Event::Heard { text: text.into(), source });
        self.m.lock().unwrap().last_command = Some(text.into());
        self.set_state(AssistantState::Thinking, text);
        let reply = self.assistant.handle(&NluInput::text(text, to_core(source))).await;
        self.finish(reply, start)
    }

    pub async fn confirm(&self, id: &str, approve: bool) -> AskResult {
        let start = Instant::now();
        let reply = if approve {
            self.set_state(AssistantState::Executing, "confirmed");
            self.assistant.confirm(id).await
        } else {
            self.assistant.reject(id)
        };
        self.finish(reply, start)
    }

    pub async fn call_tool(&self, tool: &str, args: serde_json::Value) -> AskResult {
        let start = Instant::now();
        let args = match args {
            serde_json::Value::Object(m) => m.into_iter().collect(),
            _ => Default::default(),
        };
        self.set_state(AssistantState::Executing, tool);
        let reply = self.assistant.call_tool(tool, args).await;
        self.finish(reply, start)
    }

    fn finish(&self, reply: Reply, start: Instant) -> AskResult {
        for a in &reply.actions {
            self.emit(Event::ToolFinished { record: a.clone() });
        }
        if let Some(p) = &reply.pending {
            self.emit(Event::ConfirmationRequired { pending: p.clone() });
        }
        self.emit(Event::Reply { text: reply.text.clone(), route: route_name(reply.route).into() });
        {
            let mut m = self.m.lock().unwrap();
            m.last_reply = Some(reply.text.clone());
            if let Some(a) = reply.actions.last() {
                m.last_action = Some(format!("{} ({:?})", a.tool, a.outcome).to_lowercase());
            }
        }
        self.set_state(AssistantState::Idle, "");
        AskResult {
            reply: reply.text,
            actions: reply.actions,
            pending: reply.pending,
            route: route_name(reply.route).into(),
            elapsed_ms: start.elapsed().as_millis() as u64,
        }
    }

    /// Ask the voice service (via the VoiceControl topic) to speak text.
    pub fn speak(&self, text: &str) -> String {
        self.speak_opts(text, false)
    }

    /// Speak, optionally listening for an answer straight afterwards.
    pub fn speak_opts(&self, text: &str, listen_after: bool) -> String {
        let id = self.next_utterance_id();
        self.emit(Event::VoiceControl {
            command: VoiceCommand::Speak { text: text.into(), utterance_id: id.clone(), listen_after },
        });
        id
    }

    pub fn status(&self) -> StatusReport {
        let m = self.snapshot();
        let mut components = vec![ComponentStatus {
            name: "daemon".into(),
            status: HealthStatus::Ok,
            detail: format!("pid {}", std::process::id()),
        }];
        components.push(ComponentStatus {
            name: "ai".into(),
            status: if self.assistant.provider_name() == "none" { HealthStatus::Disabled } else { HealthStatus::Unknown },
            detail: self.assistant.provider_name().into(),
        });
        match &m.voice {
            Some(v) => components.extend(v.components.iter().map(|c| ComponentStatus {
                name: format!("voice.{}", c.name),
                ..c.clone()
            })),
            None => components.push(ComponentStatus {
                name: "voice".into(),
                status: if self.config.voice.enabled { HealthStatus::Unavailable } else { HealthStatus::Disabled },
                detail: if self.config.voice.enabled { "not running".into() } else { String::new() },
            }),
        }
        let tools_total = self.assistant.gate().tools().all_names().len();
        let tools_enabled = self
            .assistant
            .gate()
            .tools()
            .all_names()
            .iter()
            .filter(|t| !self.assistant.gate().is_disabled(t))
            .count();
        StatusReport {
            version: env!("CARGO_PKG_VERSION").into(),
            pid: std::process::id(),
            uptime_s: self.started.elapsed().as_secs(),
            state: m.state,
            components,
            ai_provider: self.assistant.provider_name().into(),
            ai_model: match self.config.ai.provider {
                arc_config::ProviderKind::Local => self.config.ai.local.model.clone(),
                arc_config::ProviderKind::Openai => self.config.ai.openai.model.clone(),
                arc_config::ProviderKind::Anthropic => self.config.ai.anthropic.model.clone(),
                arc_config::ProviderKind::None => String::new(),
            },
            voice_mode: m.voice.as_ref().map(|v| v.mode),
            tools_enabled,
            tools_total,
            active_workspace: None,
            active_window: None,
            cpu_percent: None,
            mem_percent: None,
            gpu_percent: None,
            mic_muted: None,
            last_command: m.last_command,
            last_reply: m.last_reply,
            last_action: m.last_action,
            recent_errors: m.recent_errors,
            rss_kb: rss_kb(),
        }
    }

    pub fn bar_status(&self) -> BarStatus {
        let m = self.snapshot();
        let voice_ok = m.voice.as_ref().map(|v| v.running).unwrap_or(false);
        let (text, class) = match m.state {
            AssistantState::Idle if !voice_ok && self.config.voice.enabled => ("󰍭", "idle voice-off"),
            AssistantState::Idle => ("󰍬", "idle"),
            AssistantState::Listening => ("󰍬", "listening"),
            AssistantState::Thinking => ("󰔟", "thinking"),
            AssistantState::Executing => ("󰑮", "executing"),
            AssistantState::Speaking => ("󰕾", "speaking"),
            AssistantState::Error => ("󰀦", "error"),
        };
        let mut tooltip = format!("Arc: {}", m.state.label());
        if let Some(v) = &m.voice {
            let mode = match v.mode {
                VoiceMode::PushToTalk => "push to talk",
                VoiceMode::WakeWord => "wake word",
                VoiceMode::Continuous => "continuous",
            };
            tooltip.push_str(&format!("\nVoice: {mode}"));
        }
        if let Some(c) = &m.last_command {
            tooltip.push_str(&format!("\nLast: {c}"));
        }
        BarStatus { state: m.state, text: text.into(), tooltip, class: class.into(), mic_muted: None }
    }

    /// Write the bar status atomically (rename), so watchers never read a
    /// partial file.
    pub fn write_bar(&self) {
        self.write_bar_status(&self.bar_status());
    }

    fn write_bar_status(&self, status: &BarStatus) {
        let Some(path) = &self.bar_file else { return };
        let json = match serde_json::to_string(status) {
            Ok(j) => j,
            Err(_) => return,
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, path)).is_err() {
            tracing::debug!(path = %path.display(), "could not write bar status");
        }
    }
}

fn rss_kb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse().ok()
}
