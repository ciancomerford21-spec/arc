//! Shared daemon state: the assistant, current state machine, event bus,
//! status reporting and the bar status file.

use arc_config::Config;
use arc_config::paths;
use arc_core::{Assistant, InputSource as CoreSource, LastCall, NluInput, Reply, Route};
use arc_memory::MemoryStore;
use arc_proto::{
    AskResult, AssistantState, BarStatus, ComponentStatus, Event, HealthStatus, InputSource, StatusReport,
    VoiceCommand, VoiceMode,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

/// A turn at least this long gets a spoken completion line.
///
/// Above SLOW_TURN_SPEAKS_AT so the "still working" headsup and the "finished"
/// notice never fire together as two speeches for one turn.
const COMPLETION_SPEAKS_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

/// What actually happened, said plainly.
///
/// Reads the tool outcomes rather than the reply, because the reply is prose
/// and can be truncated mid-sentence. Worst outcome wins: a turn that built a
/// project and then failed a test must not be announced as finished.
fn completion_line(actions: &[arc_proto::ActionRecord]) -> Option<String> {
    // Nothing to report unless something was actually done.
    let work: Vec<_> = actions.iter().filter(|a| a.duration_ms > 0).collect();
    if work.is_empty() {
        return None;
    }
    if work.iter().any(|a| a.outcome == arc_proto::ActionOutcome::Failed) {
        return Some("That finished, but part of it failed. I have the details.".into());
    }
    // Success of the CHILD PROCESS is not the same as success of the WORK. The
    // agent exits 0 even when the task it was given could not be done -- a
    // dependency that does not exist, a build that would not pass -- and it
    // says so in prose. The tool's own `status` field is the sharper signal, so
    // it wins over the exit code, which is what this got wrong first: a run
    // whose install failed was announced as "everything ran".
    if work.iter().any(|a| a.data.get("status").and_then(|s| s.as_str()) == Some("failed")) {
        return Some("That didn't fully work out. I have the details.".into());
    }
    if work.iter().any(|a| a.outcome == arc_proto::ActionOutcome::Cancelled) {
        return Some("That stopped before it finished.".into());
    }
    if work.iter().any(|a| a.outcome == arc_proto::ActionOutcome::AwaitingConfirmation) {
        return Some("I need your OK before I finish that.".into());
    }
    let done = work.iter().filter(|a| a.outcome == arc_proto::ActionOutcome::Success).count();
    if done == 0 {
        return None;
    }
    // No tool names or counts here: speech would read "underscore code" aloud,
    // and "you asked for 1" tells the user nothing they can act on. They want
    // to know the wait is over and the work is on disk.
    let _ = done;
    Some("That's finished. Everything ran and it's all on disk. Have a look at it.".into())
}

/// How long a turn may run before Arc says something.
///
/// Measured: the `code` tool takes 2-4 minutes on a small project, and until
/// it finished the user heard nothing at all, so a request that was working
/// looked dead. Six seconds is past the point where a normal reply has
/// already started being spoken (median 2.1s, slow tool calls 3-6s), so this
/// only fires for genuinely long turns.
const SLOW_TURN_SPEAKS_AT: std::time::Duration = std::time::Duration::from_secs(6);

/// The one line said when a turn runs long. Deliberately vague: at this point
/// the tool has been chosen but nothing has happened yet, and naming a step it
/// may not reach would be a claim Arc cannot support.
const SLOW_TURN_HEADSUP: &str = "On it. This one takes a minute.";

fn next_progress_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
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
        let memory_path = std::env::var_os("ARC_MEMORY_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| paths::data_dir().join("memory"));
        let store = MemoryStore::new(memory_path);
        // Per-tool classifications, edited from the Arc app's tool list. The
        // file is the only place they live, so this is what makes a
        // classification survive a daemon restart.
        let classes = arc_config::classification::ClassifiedTools::load_default();
        if !classes.map().is_empty() {
            tracing::info!(path = %classes.path().display(), count = classes.map().len(), "tool classifications loaded");
        }
        let mut assistant = Assistant::from_config_with_classes(&config, Some(Arc::new(store)), classes)?;
        // Tools Arc made for itself. Only the daemon attaches the directory,
        // so tests and one-off library use never read or write the real ones.
        // Before automations, which may call them.
        let tools_dir = arc_config::paths::custom_tools_dir();
        let custom = assistant.gate().tools().custom().clone();
        for p in custom.attach_dir(tools_dir.clone()) {
            tracing::warn!("self-made tool not loaded: {p}");
        }
        tracing::info!(count = custom.names().len(), path = %tools_dir.display(), "self-made tools loaded");
        // A script lowered to safe was lowered by someone who read *that*
        // text. If it has been edited since, the approval does not carry over.
        for t in assistant.gate().forget_classifications(&custom.changed_since_created()) {
            tracing::warn!(tool = %t, "self-made script changed since it was created; its classification was reset");
        }
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
        // Stream each agent step to subscribers (the Arc app draws the thought
        // process from these). A send with no subscribers is not an error.
        let tx = events.clone();
        assistant.set_trace(std::sync::Arc::new(move |e| {
            let _ = tx.send(e);
        }));
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

    /// A cheap, cloneable way to make Arc say something from a background task.
    ///
    /// The events sender is behind an `Arc` already, so this hands out a
    /// closure over a clone of it rather than over `self` -- `Daemon` owns a
    /// `Mutex` and an `Assistant` and is not something to clone per turn just
    /// to announce that a turn is slow.
    fn speak_handle(&self) -> impl Fn(String) + Send + 'static {
        let events = self.events.clone();
        move |text| {
            tracing::info!("speaking progress update: {text}");
            let id = format!("progress-{}", next_progress_id());
            let _ = events.send(Event::VoiceControl {
                command: VoiceCommand::Speak { text, utterance_id: id, listen_after: false },
            });
        }
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
        // A long turn used to be silent from the user's point of view: the
        // `code` tool runs for minutes, and nothing was spoken until it
        // finished, so a request that was in fact working looked dead. This
        // speaks a short heads-up if the turn is still running after
        // SLOW_TURN_SPEAKS_AT, then keeps quiet -- a line every thirty seconds
        // would be worse than the silence it fixes.
        let speak = self.speak_handle();
        let watchdog = tokio::spawn(async move {
            tokio::time::sleep(SLOW_TURN_SPEAKS_AT).await;
            speak(SLOW_TURN_HEADSUP.to_string());
        });
        let reply = self.assistant.handle(&NluInput::text(text, to_core(source))).await;
        watchdog.abort();
        self.finish(reply, start)
    }

    /// The full tool list as the UI and CLI see it: each tool's effective risk
    /// (after the user's classification), the level the code declares, and
    /// whether the two differ.
    pub fn tool_list(&self) -> Vec<arc_proto::ToolInfo> {
        let g = self.assistant.gate();
        g.tools()
            .specs()
            .into_iter()
            .map(|s| {
                let default_risk = g.base_risk_of(&s.name).unwrap_or(arc_proto::RiskLevel::Safe);
                // The tool's level, not the risk of calling it with no
                // arguments: shell_exec with no command is harmless, which is
                // why the list used to call it "safe" while it asks before
                // most real commands.
                let risk = g.classification(&s.name).unwrap_or(default_risk);
                let enabled = !g.is_disabled(&s.name);
                let made = g.tools().custom().get(&s.name).map(|d| d.kind().to_string());
                arc_proto::ToolInfo {
                    category: match &made {
                        Some(k) => format!("self-made {k}"),
                        None => s.name.split('_').next().unwrap_or("").into(),
                    },
                    made_by_arc: made,
                    lowerable: default_risk != arc_proto::RiskLevel::Dangerous
                        || g.tools().by_name(&s.name).is_some_and(|t| t.user_may_lower()),
                    unavailable_reason: (!enabled).then(|| "disabled in configuration".into()),
                    reclassified: g.classification(&s.name).is_some(),
                    name: s.name,
                    description: s.description,
                    risk,
                    default_risk,
                    parameters: s.parameters,
                    enabled,
                }
            })
            .collect()
    }

    /// Change one tool's classification, from the UI's dropdown. Persisted by
    /// the gate before this returns, so a failure here means nothing changed.
    pub fn set_tool_class(
        &self,
        tool: &str,
        level: Option<arc_proto::RiskLevel>,
    ) -> Result<arc_proto::ToolInfo, String> {
        let g = self.assistant.gate();
        g.set_classification(tool, level)?;
        tracing::info!(tool, level = %level.map(|l| l.to_string()).unwrap_or_else(|| "default".into()), "classification changed");
        self.tool_list().into_iter().find(|t| t.name == tool).ok_or_else(|| format!("unknown tool `{tool}`"))
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
        // A long task that ends quietly is indistinguishable from one that
        // died, especially after an 85-second silence. The reply itself is not
        // a reliable signal: it is the model's prose, and on a long coding turn
        // it is routinely truncated to a summary ending "Want the rest?".
        // This line is built from the real ActionOutcome, so it says finished
        // because the tool finished.
        if start.elapsed() >= COMPLETION_SPEAKS_AFTER {
            if let Some(line) = completion_line(&reply.actions) {
                tracing::info!("speaking completion notice: {line}");
                let events = self.events.clone();
                let _ = events.send(Event::VoiceControl {
                    command: VoiceCommand::Speak {
                        text: line,
                        utterance_id: format!("done-{}", next_progress_id()),
                        listen_after: false,
                    },
                });
            }
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
        // Report what the provider actually did, not a probe. A probe here
        // would spend a real request (and eat a possible 500) to colour one
        // line, and the answer would describe a synthetic call rather than
        // the traffic Arc is really serving. Before this the line read
        // "unknown" for every configured provider, which is true of nothing
        // and useful for no one.
        let (ai_status, ai_detail) = match self.assistant.last_call() {
            None => (HealthStatus::Disabled, String::new()),
            Some(LastCall::Never) => (HealthStatus::Unknown, "no request yet".into()),
            Some(LastCall::Ok) => (HealthStatus::Ok, self.assistant.provider_name().into()),
            Some(LastCall::Failed(why)) => {
                (HealthStatus::Unavailable, format!("{}: {why}", self.assistant.provider_name()))
            }
        };
        components.push(ComponentStatus { name: "ai".into(), status: ai_status, detail: ai_detail });
        match &m.voice {
            Some(v) => components.extend(
                v.components
                    .iter()
                    .map(|c| ComponentStatus { name: format!("voice.{}", c.name), ..c.clone() }),
            ),
            None => components.push(ComponentStatus {
                name: "voice".into(),
                status: if self.config.voice.enabled {
                    HealthStatus::Unavailable
                } else {
                    HealthStatus::Disabled
                },
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
                arc_config::ProviderKind::Openai => self.config.ai.openai.model.clone(),
                arc_config::ProviderKind::Hermes => self.config.ai.hermes.model.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use arc_proto::ActionOutcome;

    #[test]
    fn a_slow_turn_says_something_before_it_finishes() {
        // The bug this fixes: a `code` task runs for minutes and used to be
        // completely silent, so a build that was working looked dead. Assert
        // the threshold sits above a normal reply and below a long tool call.
        assert!(
            SLOW_TURN_SPEAKS_AT > std::time::Duration::from_millis(2100),
            "the headsup would cut into a normal reply"
        );
        assert!(
            SLOW_TURN_SPEAKS_AT < std::time::Duration::from_secs(30),
            "that is a long silence to leave the user in"
        );
        // It must not promise a step: at six seconds the tool is chosen but
        // nothing has happened yet.
        assert!(!SLOW_TURN_HEADSUP.contains("test") && !SLOW_TURN_HEADSUP.contains("build"));
    }

    #[test]
    fn progress_ids_are_unique() {
        // Each spoken headsup needs its own id, or the voice service will treat
        // two of them as the same utterance and cut one off.
        let a = next_progress_id();
        let b = next_progress_id();
        assert_ne!(a, b);
    }

    fn act(tool: &str, outcome: ActionOutcome, ms: u64) -> arc_proto::ActionRecord {
        arc_proto::ActionRecord {
            tool: tool.into(),
            args: serde_json::json!({}),
            risk: arc_proto::RiskLevel::Caution,
            outcome,
            summary: String::new(),
            warning: None,
            data: serde_json::Value::Null,
            duration_ms: ms,
        }
    }

    /// The point of the line: it is built from tool outcomes, so it cannot
    /// claim success when the run failed, whatever the model's prose says.
    #[test]
    fn a_failed_step_is_never_announced_as_finished() {
        let line = completion_line(&[
            act("code", ActionOutcome::Success, 60_000),
            act("code", ActionOutcome::Failed, 5_000),
        ])
        .unwrap();
        assert!(line.contains("failed"), "{line}");
        assert!(!line.contains("done"), "worst outcome must win: {line}");
    }

    #[test]
    fn a_pending_confirmation_says_so_instead_of_claiming_done() {
        let line = completion_line(&[act("code", ActionOutcome::AwaitingConfirmation, 40_000)]).unwrap();
        assert!(line.contains("OK"), "{line}");
    }

    #[test]
    fn a_cancelled_run_does_not_claim_success() {
        let line = completion_line(&[act("code", ActionOutcome::Cancelled, 40_000)]).unwrap();
        assert!(line.contains("stopped"), "{line}");
    }

    #[test]
    fn a_successful_long_run_announces_completion() {
        let line = completion_line(&[act("code", ActionOutcome::Success, 85_000)]).unwrap();
        assert!(line.contains("finished"), "must say the wait is over: {line}");
        // Tool names must never reach speech.
        assert!(!line.contains("code"), "would be read aloud as a word: {line}");
    }

    #[test]
    fn a_turn_that_did_nothing_says_nothing() {
        // Reading the battery is not worth announcing, and the reply already
        // covers it. duration_ms > 0 filters these out.
        assert_eq!(completion_line(&[act("power_info", ActionOutcome::Success, 0)]), None);
        assert_eq!(completion_line(&[]), None);
    }

    #[test]
    fn the_completion_notice_never_collides_with_the_headsup() {
        // Both fire off one turn; if the thresholds overlapped the user would
        // hear "on it" and "that's done" for the same request.
        assert!(
            COMPLETION_SPEAKS_AFTER > SLOW_TURN_SPEAKS_AT,
            "a turn between the two thresholds would speak twice"
        );
    }

    /// The one that slipped through: the agent process exited 0 while the
    /// task it was handed had failed. Exit status is not task status.
    #[test]
    fn a_child_exit_zero_with_a_failed_task_is_not_announced_as_finished() {
        let mut a = act("code", ActionOutcome::Success, 40_000);
        a.data = serde_json::json!({"status": "failed"});
        let line = completion_line(&[a]).unwrap();
        assert!(!line.contains("everything ran"), "{line}");
        assert!(line.contains("didn't fully work"), "{line}");
    }

    #[test]
    fn a_reported_ok_task_still_announces_completion() {
        let mut a = act("code", ActionOutcome::Success, 40_000);
        a.data = serde_json::json!({"status": "done"});
        assert!(completion_line(&[a]).unwrap().contains("finished"));
    }
}
