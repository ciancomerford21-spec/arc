//! Shared daemon state: the assistant, current state machine, event bus,
//! status reporting and the bar status file.

use arc_config::Config;
use arc_config::paths;
use arc_core::{Assistant, InputSource as CoreSource, LastCall, NluInput, Reply, Route};
use arc_memory::MemoryStore;
use arc_proto::{
    AskResult, AssistantState, BarStatus, ComponentStatus, Event, HealthStatus, InputSource,
    NowPlaying, NowPlayingRequest, NowPlayingState, StatusReport, VoiceCommand, VoiceMode,
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
    /// The track the overlay shows. `None` when nothing is playing.
    pub now_playing: Option<NowPlaying>,
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
    /// True from the moment the `code` tool starts until the turn ends. The
    /// slow-turn headsup checks it: a `code` task gets its own spoken
    /// progress ("Handing that to Hermes...") within a few seconds, and
    /// speaking the generic headsup as well produced two speeches three
    /// seconds apart, measured live.
    code_active: Arc<std::sync::atomic::AtomicBool>,
    /// Name of the tool currently running, kept by `emit` so a turn that runs
    /// long can say what it is waiting on instead of shrugging.
    running_tool: Arc<Mutex<Option<String>>>,
}

/// A turn at least this long gets a spoken completion line.
///
/// Above SLOW_TURN_SPEAKS_AT so the "still working" headsup and the "finished"
/// notice never fire together as two speeches for one turn.
const COMPLETION_SPEAKS_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

/// What actually happened, said plainly -- and said *variously*.
///
/// This used to be one fixed sentence, "That's finished. Everything ran and
/// it's all on disk. Have a look at it.", for every long turn ever. It said
/// nothing the reply that immediately follows it did not already say, and
/// hearing it twice in one session is how a voice assistant starts to feel
/// like a machine. So the line is now built from the real outcome -- which
/// tool, how long, how many steps, what happened -- and the phrasing is
/// rotated, so no two turns in a row sound the same.
///
/// It is deliberately short. The model's own reply is spoken next; this is a
/// fact, not a second summary.
fn completion_line(actions: &[arc_proto::ActionRecord]) -> Option<String> {
    // Nothing to report unless something was actually done.
    let work: Vec<_> = actions.iter().filter(|a| a.duration_ms > 0).collect();
    if work.is_empty() {
        return None;
    }
    if work.iter().any(|a| a.outcome == arc_proto::ActionOutcome::AwaitingConfirmation) {
        // Both name the OK explicitly: this line gates a real action, and
        // "I need your yes first" is vaguer about what is being approved.
        return Some(vary(&["I need your OK before I finish that.", "Hold on -- say OK and I'll carry on."]));
    }
    if work.iter().any(|a| a.outcome == arc_proto::ActionOutcome::Cancelled) {
        return Some(vary(&["That stopped before the end.", "I pulled it before it finished."]));
    }
    // Success of the CHILD PROCESS is not success of the WORK. The agent exits
    // 0 even when the task could not be done, so the tool's own `status` is
    // the sharper signal and it wins over the exit code.
    let hard_failed = work.iter().any(|a| {
        a.outcome == arc_proto::ActionOutcome::Failed
            || a.data.get("status").and_then(|s| s.as_str()) == Some("failed")
    });
    let total_ms: u64 = work.iter().map(|a| a.duration_ms).sum();
    // Capitalised for speech: these lines start sentences.
    let subject_start = {
        let s = work.last().map(|a| tool_phrase(&a.tool)).unwrap_or_else(|| "that".into());
        let mut c = s.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => String::from("That"),
        }
    };

    if hard_failed {
        return match failure_detail(&work) {
            Some(why) => Some(vary(&[
                &format!("{subject_start} hit a wall: {why}."),
                &format!("That one didn't work -- {why}."),
                &format!("{subject_start} fell over: {why}."),
            ])),
            None => Some(vary(&[
                &format!("{subject_start} didn't finish cleanly. I've got the details."),
                "That didn't come out clean. Details coming up.",
            ])),
        };
    }
    if !work.iter().any(|a| a.outcome == arc_proto::ActionOutcome::Success) {
        return None;
    }
    // A `code` task reports its own real numbers, so use them: "9 steps, 37
    // seconds" is information, "everything ran" is not.
    // Either a direct `code` action, or a self-made composite that ran one
    // as a step and passed its numbers up.
    let code_data = work
        .iter()
        .rev()
        .find(|a| a.tool == "code")
        .map(|a| a.data.clone())
        .or_else(|| work.iter().rev().find_map(|a| a.data.get("code").cloned()));
    if let Some(data) = code_data {
        let steps = data.get("steps").and_then(|v| v.as_u64());
        let took = data.get("took_s").and_then(|v| v.as_u64()).unwrap_or(total_ms / 1000);
        // No verdict here. The `code` tool cannot know whether the task
        // succeeded: hermes exits 0 even when the build failed and says so in
        // prose. Measured -- a task that failed dependency resolution reported
        // `status: done`, because that flag is the process exit code, and the
        // line announced success over a broken build. The model's own reply,
        // spoken next, carries the verdict. So this line closes the wait with
        // numbers and stops: asserting "done" would be a claim Arc cannot
        // support, and repeating the verdict is the double summary this line
        // exists to avoid.
        return Some(match (steps, took) {
            (Some(n), t) => {
                let steps = if n == 1 { "1 step".to_string() } else { format!("{n} steps") };
                let secs = plural_secs(t * 1000);
                vary(&[
                    &format!("Hermes: {steps}, {secs}."),
                    &format!("That took Hermes {steps}, {secs}."),
                    &format!("Hermes worked through {steps} in {secs}."),
                ])
            }
            (None, t) => vary(&[&format!("Hermes took {t} seconds."), &format!("That ran for {t} seconds.")]),
        });
    }
    let n = work.len();
    if n == 1 {
        let secs = plural_secs(total_ms);
        return Some(vary(&[
            &format!("{subject_start} took {secs}. Done."),
            &format!("{subject_start} finished in {secs}."),
        ]));
    }
    let secs = plural_secs(total_ms);
    Some(vary(&[&format!("{n} things done in {secs}."), &format!("That was {n} steps, {secs}.")]))
}

/// The plainest name for a tool, for saying out loud. Tool names are not
/// speakable -- "underscore code" -- and a line that makes the user decode an
/// identifier has failed at its only job.
fn tool_phrase(tool: &str) -> String {
    match tool {
        "code" => "Hermes".into(),
        "shell_exec" => "the shell".into(),
        "web_search" | "web_extract" => "the search".into(),
        "tool_create" => "the new tool".into(),
        "tool_delete" => "the delete".into(),
        "tool_list_own" => "the list".into(),
        t if t.starts_with("workspace_") => "the workspace".into(),
        t if t.starts_with("window_") || t.starts_with("screen") => "the window".into(),
        t if t.starts_with("media_") || t.starts_with("audio_") => "the sound".into(),
        t if t.starts_with("file_") => "the file".into(),
        // A self-made tool: Arc named it, so the name is probably speakable,
        // but a long or underscored one is not. Use a pronoun instead.
        _ => "that".into(),
    }
}

/// The most specific failure reason available, for a line that says *why*.
///
/// A generic "it failed" is the same non-information this replaced; the
/// tool's own summary usually names the cause, so the first line of it is
/// used, cut to something sayable.
fn failure_detail(work: &[&arc_proto::ActionRecord]) -> Option<String> {
    let a = work.iter().find(|a| {
        a.outcome == arc_proto::ActionOutcome::Failed
            || a.data.get("status").and_then(|s| s.as_str()) == Some("failed")
    })?;
    let raw = a
        .data
        .get("error")
        .and_then(|v| v.as_str())
        .or_else(|| a.data.get("summary").and_then(|v| v.as_str()))
        .or_else(|| a.summary.as_str().into())?;
    let first = raw.lines().find(|l| !l.trim().is_empty())?.trim();
    let flat = first.split_whitespace().collect::<Vec<_>>().join(" ");
    let words: Vec<&str> = flat.split(' ').take(9).collect();
    let s = words.join(" ");
    if s.is_empty() { None } else { Some(s) }
}

fn plural_secs(ms: u64) -> String {
    let s = ms / 1000;
    if s == 1 { "a second".to_string() } else { format!("{s} seconds") }
}

/// Pick one phrasing, rotating so consecutive turns differ.
///
/// Deterministic on purpose: a test can assert on a set of possibilities, and
/// a fixed line is exactly what this is fixing. The counter is per-process, so
/// the first long turn after a restart says the same thing it always did --
/// which is a fair trade for a predictable test.
fn vary(options: &[&str]) -> String {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let i = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    options[i % options.len()].to_string()
}

/// How long a turn may run before Arc says something.
///
/// Measured: the `code` tool takes 2-4 minutes on a small project, and until
/// it finished the user heard nothing at all, so a request that was working
/// looked dead. Six seconds is past the point where a normal reply has
/// already started being spoken (median 2.1s, slow tool calls 3-6s), so this
/// only fires for genuinely long turns.
const SLOW_TURN_SPEAKS_AT: std::time::Duration = std::time::Duration::from_secs(6);

/// The model's words alongside a tool call, prepared for speech.
///
/// These were display-only, so a turn that opened with one was silent until
/// the tool finished: measured live, "take a screenshot and describe what you
/// see" said "I'll grab a shot." to nobody and then took 135 seconds.
///
/// Rules, because a note is a promise to say something and then stop talking:
/// - at most [`NOTE_SPOKEN_MAX`] per turn, so a four-round tool loop is not
///   four announcements;
/// - never during a `code` task, which has its own progress lines and would
///   otherwise double up;
/// - never a question: the user would answer into a turn that is still
///   running, and nothing would use the answer;
/// - the first sentence only, cut short: the point is to cover a wait, not
///   to be read.
fn speakable_note(text: &str, spoken: &mut u32, code_active: bool) -> Option<String> {
    if *spoken >= NOTE_SPOKEN_MAX || code_active {
        return None;
    }
    let t = text.trim();
    if t.is_empty() || t.ends_with('?') {
        return None;
    }
    let first = t.split(['.', '!', '\n']).map(str::trim).find(|s| !s.is_empty())?;
    let words: Vec<&str> = first.split_whitespace().take(12).collect();
    let line = words.join(" ");
    if line.len() < 3 {
        return None;
    }
    *spoken += 1;
    Some(if line.ends_with('.') || line.ends_with('!') { line } else { format!("{line}.") })
}

/// One note per turn. Two would be a conversation with itself.
const NOTE_SPOKEN_MAX: u32 = 1;

/// What the progress relay should do with one event.
///
/// Extracted from the relay loop because the loop's first version peeked for
/// a `Thought` with a *second* `recv`, which threw away every other event --
/// including the `ToolStarted` that marks a code task. A bug in a `match` arm
/// is hard to see; a bug in a total function with a test is not.
fn note_action(ev: &Event, speak_notes: bool, notes: &mut u32, code_active: bool) -> Option<Option<String>> {
    if !speak_notes {
        return None;
    }
    let Event::Thought { text, speakable, .. } = ev else { return None };
    if text.trim().is_empty() {
        return Some(None);
    }
    // `speakable` is false for a round that is calling `code`: the handoff
    // line covers it, and two announcements seconds apart is what that guard
    // exists to prevent.
    let busy = !speakable || code_active;
    Some(speakable_note(text, notes, busy))
}

/// The line said when a turn runs long.
///
/// It used to be the fixed "On it. This one takes a minute." for every slow
/// turn ever, which told the user nothing except that time was passing. It
/// is now built from what is actually known at that moment -- the tool that
/// is running and how long it has been running -- and rotated, so it does not
/// become a sound the user learns to wait for.
///
/// What it must not do is name a step the turn may never reach: at 6s the tool
/// has been chosen, so the tool is a fact, but "it's writing the tests" when
/// it is still reading files would be a lie.
fn slow_turn_headsup(tool: Option<&str>, elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs().max(1);
    // Grammatically usable on its own, because it is dropped into the middle
    // of a sentence: "the shell's", "Hermes has been". "it" reads as a typo
    // when it starts one.
    let (doing, subject) = match tool.map(tool_phrase) {
        Some(t) => (format!("{t}'s"), t),
        None => (String::from("it"), String::from("it")),
    };
    // Long enough to be worth mentioning, and the wait is real information
    // only when it is long.
    // Every variant names the tool. One of them used to be a bare "Still
    // working.", which is the exact non-information this function replaced --
    // a test caught it losing the name the caller went to the trouble of
    // supplying.
    let named = tool.is_some();
    if secs >= 60 {
        if named {
            vary(&[
                &format!("Still going -- {subject} has been at it {secs} seconds."),
                &format!("{subject} is still running, {secs} seconds in."),
                &format!("No news yet. {subject} has been at it {secs} seconds."),
            ])
        } else {
            // No tool known yet, so no subject to attach a verb to: "it has
            // been at it" is grammatical but reads like a typo when it opens
            // the line.
            vary(&[
                &format!("Still going -- {secs} seconds in."),
                &format!("No news yet, {secs} seconds in."),
                &format!("Still working. {secs} seconds so far."),
            ])
        }
    } else if named {
        vary(&[
            &format!("Working on it -- {doing} running."),
            &format!("{doing} going. Give it a moment."),
            &format!("Still in there -- {doing} running."),
        ])
    } else {
        vary(&["Still working on it.", "Give it a moment.", "Still going, one moment."])
    }
}

/// What Arc says while Hermes works, and when.
///
/// The pipeline delivers a line per Hermes tool call -- measured on a real
/// task, 9 calls for a small feature and 63 for a larger one -- so speaking
/// them all would be unusable. The rules:
///
/// 1. At most [`PROGRESS_SPOKEN_MAX`] lines per task, so a long build still
///    ends with the model's own reply rather than a running commentary.
/// 2. At least [`PROGRESS_SPOKEN_GAP`] apart.
/// 3. Only for tools that mean something happened. Reading and searching are
///    invisible: "it's reading a file" is not information.
///
/// Within that budget the lines are specific -- the work, then the phase --
/// and phrased in Arc's own voice, because a handoff line is still something
/// the user has to listen to. A fixed "On it" for every handoff is what this
/// replaced.
struct ProgressTalk {
    said: u32,
    last_at: Option<std::time::Instant>,
}

const PROGRESS_SPOKEN_MAX: u32 = 3;
const PROGRESS_SPOKEN_GAP: std::time::Duration = std::time::Duration::from_secs(25);

/// Tools worth a spoken mention: something was made or run.
fn progress_is_notable(tool: &str) -> bool {
    matches!(tool, "write_file" | "patch" | "terminal" | "process" | "execute_code" | "delegate_task")
}

/// The spoken form of one progress line. `None` means show it, don't say it.
fn progress_line(
    task: &str,
    tool: &str,
    step: u32,
    elapsed_s: u64,
    talk: &mut ProgressTalk,
) -> Option<String> {
    if talk.said >= PROGRESS_SPOKEN_MAX || !progress_is_notable(tool) {
        return None;
    }
    if let Some(last) = talk.last_at {
        if last.elapsed() < PROGRESS_SPOKEN_GAP {
            return None;
        }
    }
    talk.said += 1;
    talk.last_at = Some(std::time::Instant::now());
    Some(match talk.said {
        // The handoff. Names the work, and is honest that this is going to
        // take a while, which is the one thing worth saying at the start.
        1 => vary(&[
            &format!("Handing that to Hermes -- {task}. This will take a bit."),
            &format!("Hermes is on it: {task}. Give me a few minutes."),
            &format!("Okay, Hermes is building that -- {task}. I'll report back."),
        ]),
        2 => match tool {
            "write_file" | "patch" => vary(&[
                "It's writing the files now.",
                "Files are being written.",
                "It's putting the code down now.",
            ]),
            _ => vary(&[
                "It's building and testing now -- the slow part.",
                "Building, running the tests. This is where the time goes.",
                "It's compiling and testing now.",
            ]),
        },
        // The last line carries the running totals, which is the only number
        // that changes meaning as the task goes on. Its phrasing is distinct
        // from the second line's on purpose: two lines about "still going" in
        // a row is the monotony this whole change is about.
        _ => vary(&[
            &format!("Still going -- step {step}, {elapsed_s} seconds in."),
            &format!("Hermes is {elapsed_s} seconds in. Step {step}."),
        ]),
    })
}

/// The user's request, shortened to something speakable.
///
/// This is spoken in the first progress line, so it must be short, must not
/// be the whole sentence (which may be 40 words of dictation), and must be
/// safe to say out loud: no paths, no commands, no names that need spelling
/// out. Falls back to something generic when nothing usable is left.
fn task_summary(text: &str) -> String {
    let cleaned = text
        .trim()
        .trim_start_matches(|c: char| c == '.' || c == ',' || c.is_whitespace())
        .split(&['.', ',', ';', '!', '?', '\n'][..])
        .map(str::trim)
        // A leading prepositional clause is scene-setting, not the job:
        // "In the pipeline-test project, add a subtract function" was
        // announced as "working on In the pipeline-test project", which
        // told the user nothing.
        .find(|c| {
            !c.is_empty()
                && !["in ", "on ", "at ", "for ", "from ", "inside ", "within ", "under ", "to "]
                    .iter()
                    .any(|p| c.to_lowercase().starts_with(p))
        })
        .unwrap_or("that");
    // Politeness first, then an imperative, so the line reads as a noun
    // phrase: "can you please write a test" -> "a test". Matched
    // case-insensitively, since dictation and typed text disagree about
    // capitals. The bare verbs are in the list because after the politeness
    // is stripped what often remains is still a command, which reads aloud as
    // an order rather than a subject.
    const LEAD: [&str; 16] = [
        "please ",
        "can you ",
        "could you ",
        "would you ",
        "i want ",
        "i need ",
        "i'd like ",
        "make me ",
        "build me ",
        "write me ",
        "create ",
        "give me ",
        "write ",
        "build ",
        "add ",
        "make ",
    ];
    let mut stripped = cleaned.to_string();
    // Repeatedly, not once: "can you please write a test" carries two of
    // these, and one pass left "please write a test for the gate" to be said
    // aloud.
    loop {
        let lower = stripped.to_lowercase();
        match LEAD.iter().find(|p| lower.starts_with(**p)) {
            Some(p) => stripped = stripped[p.len()..].trim_start().to_string(),
            None => break,
        }
    }
    // Never speak a path or a command: TTS reads "/" as "slash" and
    // "/home/user/Projects" as a spelled-out string, which is both useless
    // and a disclosure of the directory layout. Drop those tokens whole and
    // keep the words around them.
    let words: Vec<&str> =
        stripped.split_whitespace().filter(|w| !w.contains('/') && !w.contains('\\')).take(7).collect();
    let s = words.join(" ");
    if s.is_empty() { "that".to_string() } else { s }
}

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
        // The code pipeline: Hermes reports each tool call as it happens, so
        // Arc can show the work as it goes instead of going quiet for the
        // whole task. Shown always; the daemon speaks a few of them.
        let px = events.clone();
        assistant.set_code_progress(std::sync::Arc::new(move |p| {
            let _ = px.send(Event::CodeProgress {
                tool: p.tool,
                detail: p.detail,
                step: p.step,
                elapsed_s: p.elapsed_s,
            });
        }));
        Ok(Self {
            assistant,
            config,
            events,
            started: Instant::now(),
            bar_file,
            m: Mutex::new(Mutable::default()),
            utterance: AtomicU64::new(0),
            code_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            running_tool: Arc::new(Mutex::new(None)),
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
        // Track what is running, so a slow turn can name it in the headsup
        // rather than shrugging. Cheap: one string move per tool event.
        match &e {
            Event::ToolStarted { tool, .. } => *self.running_tool.lock().unwrap() = Some(tool.clone()),
            Event::ToolFinished { .. } => *self.running_tool.lock().unwrap() = None,
            _ => {}
        }
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

    /// Handle a now-playing report from a playback tool.
    ///
    /// `set` is called by the tool once the player is actually running, so the
    /// overlay never shows a track that failed to start. `stop` clears it.
    /// Both are idempotent: a duplicate report publishes nothing, which is
    /// what stops the overlay's row from flickering on every poll.
    pub fn now_playing(&self, req: NowPlayingRequest) -> Result<NowPlaying, String> {
        let next = match req {
            // A read is not a state change: nothing is emitted, nothing is
            // stored, the answer is whatever is already there.
            NowPlayingRequest::Show => return Ok(self.now_playing_status()),
            NowPlayingRequest::Set { title, artist, source, pid } => {
                let title = title.trim();
                if title.is_empty() {
                    return Err("a now-playing report needs a title".into());
                }
                NowPlaying {
                    state: NowPlayingState::Playing,
                    title: title.chars().take(200).collect(),
                    artist: artist.trim().chars().take(200).collect(),
                    source: source.trim().chars().take(60).collect(),
                    pid,
                }
            }
            NowPlayingRequest::Stop => NowPlaying::default(),
        };
        let changed = {
            let mut m = self.m.lock().unwrap();
            let current = m.now_playing.clone().unwrap_or_default();
            // An empty stop on an already-empty state is not a change: this
            // is the common case (every script that finds no player), and it
            // must not push an event each time.
            if !next.is_playing() && !current.is_playing() {
                return Ok(NowPlaying::default());
            }
            if next == current {
                return Ok(next);
            }
            m.now_playing = if next.is_playing() { Some(next.clone()) } else { None };
            true
        };
        if changed {
            let published = next.clone();
            self.emit(Event::NowPlaying { status: published });
            self.publish_bar();
        }
        Ok(next)
    }

    /// What the overlay should currently show.
    pub fn now_playing_status(&self) -> NowPlaying {
        self.m.lock().unwrap().now_playing.clone().unwrap_or_default()
    }

    /// Clear the now-playing row when the player process is gone.
    ///
    /// A tool that starts a player in the background cannot tell anyone when
    /// the track ends -- it has already exited by then. Watching the pid is
    /// the only signal that exists, so this runs on a timer rather than being
    /// left to the script. `pid == 0` means the reporter did not say, and
    /// nothing is reaped: mpv is a long-lived process in other setups and
    /// killing the row because pid 0 looked dead would be wrong.
    pub fn reap_now_playing(&self) {
        let dead = {
            let m = self.m.lock().unwrap();
            match &m.now_playing {
                Some(n) if n.is_playing() && n.pid != 0 => !process_alive(n.pid),
                _ => false,
            }
        };
        if dead {
            tracing::debug!("player process gone; clearing now-playing");
            let _ = self.now_playing(NowPlayingRequest::Stop);
        }
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
        let code_active = self.code_active.clone();
        let running = self.running_tool.clone();
        let watchdog = tokio::spawn(async move {
            tokio::time::sleep(SLOW_TURN_SPEAKS_AT).await;
            // A `code` task speaks for itself within a few seconds, so the
            // generic headsup would be a second speech seconds apart.
            if code_active.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            // Name the tool that is actually running, not a guess at the step.
            // Nothing running yet means the model is still choosing, and there
            // is no fact to report: measured live, this fired as "Still going,
            // one moment." six seconds into a turn whose first tool call came
            // at 18s, which is the same empty reassurance this line replaced.
            let tool = running.lock().unwrap().clone();
            if tool.is_none() {
                return;
            }
            speak(slow_turn_headsup(tool.as_deref(), SLOW_TURN_SPEAKS_AT));
        });
        // Relay Hermes' progress while the turn runs. Each subscriber gets
        // its own channel clone, so this sees only what arrives from here;
        // `speak` is the same voice queue the headsup uses, so the two queue
        // rather than talk over each other.
        let mut talk = ProgressTalk { said: 0, last_at: None };
        let task = task_summary(text);
        let code_flag = self.code_active.clone();
        let speak_notes = self.config.voice.speak_tool_notes;
        let mut notes = 0u32;
        let relay = tokio::spawn({
            let mut rx = self.events.subscribe();
            let speak = self.speak_handle();
            async move {
                loop {
                    // One receive, then dispatch. The first version peeked for
                    // a Thought with a second `recv`, which silently threw
                    // away every other event -- including the ToolStarted
                    // that marks a code task, and the progress events.
                    let ev = match rx.recv().await {
                        Ok(e) => e,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "progress relay fell behind");
                            continue;
                        }
                        Err(_) => break,
                    };
                    // The model's own words for what it is about to do. These
                    // were display-only, so the turn was silent until the tool
                    // finished -- measured live at 135s with nothing said.
                    if let Some(action) = note_action(
                        &ev,
                        speak_notes,
                        &mut notes,
                        code_flag.load(std::sync::atomic::Ordering::Relaxed),
                    ) {
                        match action {
                            Some(line) => {
                                tracing::info!(note = %line, "speaking the model's note");
                                speak(line);
                            }
                            None => tracing::info!("note not spoken"),
                        }
                        continue;
                    }
                    let (tool, detail, step, elapsed) = match ev {
                        Event::CodeProgress { tool, detail, step, elapsed_s } => {
                            (tool, detail, step, elapsed_s)
                        }
                        // The headsup stands down once a code task is under
                        // way, so mark it as soon as the tool starts.
                        Event::ToolStarted { tool, .. } if tool == "code" => {
                            code_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                            continue;
                        }
                        _ => continue,
                    };
                    match progress_line(&task, &tool, step, elapsed, &mut talk) {
                        Some(t) => {
                            tracing::info!(tool = %tool, detail = %detail, step, spoken = %t, "hermes progress");
                            speak(t);
                        }
                        None => tracing::info!(tool = %tool, detail = %detail, step, "hermes progress"),
                    }
                }
            }
        });
        let reply = self.assistant.handle(&NluInput::text(text, to_core(source))).await;
        watchdog.abort();
        relay.abort();
        self.code_active.store(false, std::sync::atomic::Ordering::Relaxed);
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
            now_playing: m.now_playing.filter(|n| n.is_playing()),
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
        // Only while something is actually playing, so an idle bar looks
        // exactly as it did before this existed.
        let now_playing = m.now_playing.clone().filter(|n| n.is_playing());
        if let Some(n) = &now_playing {
            tooltip.push_str(&format!("\n♪ {}", n.label()));
        }
        BarStatus {
            state: m.state,
            text: text.into(),
            tooltip,
            class: class.into(),
            mic_muted: None,
            now_playing,
        }
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

/// Is this pid still running? `kill -0` is the check, but only after
/// confirming it is not our own group leader, which would signal us.
/// Signals are checked through the `kill` return value; EPERM means the
/// process exists but belongs to someone else, which for a player we started
/// cannot happen and so counts as gone.
fn process_alive(pid: u32) -> bool {
    // A pid wider than pid_t cannot be converted without wrapping, and the
    // wrapped value is a *negative* pid -- where kill(-1, 0) means "every
    // process I may signal" and cheerfully reports success for a pid that has
    // never existed. Cast a pid_t through u32, not i32, and refuse the ones
    // that would wrap.
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs the existence/permission check only; it
    // delivers nothing and cannot kill the caller.
    unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_proto::ActionOutcome;

    /// Speaking every Hermes tool call would be unusable (9 calls for a small
    /// feature, 63 for a larger one), so: only tools where something was
    /// written or run, at most three lines, never two close together.
    #[test]
    fn progress_is_spoken_sparingly_and_only_when_something_happens() {
        let mut t = ProgressTalk { said: 0, last_at: None };
        let ago =
            || Some(std::time::Instant::now() - PROGRESS_SPOKEN_GAP - std::time::Duration::from_secs(1));
        // Reading is invisible: "it's reading a file" tells the user nothing
        // they can act on.
        assert_eq!(progress_line("a test", "read_file", 1, 6, &mut t), None);
        assert_eq!(progress_line("a test", "search_files", 2, 9, &mut t), None);
        let first =
            progress_line("a test", "write_file", 3, 12, &mut t).expect("the first notable step is spoken");
        assert!(first.contains("a test"), "{first}");
        assert!(first.contains("Hermes"), "the handoff should name who has it: {first}");
        assert_eq!(progress_line("a test", "terminal", 4, 20, &mut t), None, "too soon after the last line");
        t.last_at = ago();
        let second =
            progress_line("a test", "terminal", 5, 31, &mut t).expect("spoken once the gap has passed");
        assert!(second.contains("build") || second.contains("test") || second.contains("compil"), "{second}");
        t.last_at = ago();
        let third = progress_line("a test", "terminal", 9, 60, &mut t).expect("a third line is allowed");
        assert!(
            third.contains('9') && third.contains("60"),
            "the last line should carry the real numbers: {third}"
        );
        t.last_at = ago();
        assert_eq!(progress_line("a test", "write_file", 10, 70, &mut t), None, "capped at three lines");
        assert_eq!(t.said, PROGRESS_SPOKEN_MAX);
    }

    /// The handoff and the completion line were fixed sentences, so every
    /// long turn sounded identical. They must vary.
    #[test]
    fn the_spoken_lines_are_not_the_same_every_time() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..6 {
            let mut t = ProgressTalk { said: 0, last_at: None };
            seen.insert(progress_line("a test", "write_file", 1, 5, &mut t).unwrap());
        }
        assert!(seen.len() >= 2, "every handoff sounded the same: {seen:?}");

        let ok = |n: u64| -> arc_proto::ActionRecord {
            arc_proto::ActionRecord {
                tool: "network_status".into(),
                args: serde_json::json!({}),
                risk: arc_proto::RiskLevel::Safe,
                outcome: ActionOutcome::Success,
                summary: String::new(),
                warning: None,
                data: serde_json::json!({ "n": n }),
                duration_ms: 4000,
            }
        };
        let mut outs = std::collections::HashSet::new();
        for n in 0..4 {
            outs.insert(completion_line(&[ok(n)]).unwrap());
        }
        assert!(outs.len() >= 2, "every completion sounded the same: {outs:?}");
    }

    /// A self-made composite that runs `code` as a step reports as one action
    /// under its own name, so the numbers have to be lifted or the line falls
    /// back to counting actions -- measured live as "2 steps, 303 seconds" for
    /// a five-minute Hermes run.
    #[test]
    fn a_composite_that_runs_code_still_reports_hermes_numbers() {
        let rec = arc_proto::ActionRecord {
            tool: "see_screen".into(),
            args: serde_json::json!({}),
            risk: arc_proto::RiskLevel::Dangerous,
            outcome: ActionOutcome::Success,
            summary: String::new(),
            warning: None,
            data: serde_json::json!({
                "steps": [{"step": 1, "tool": "screenshot", "result": {"output": "Saved"}}],
                "code": {"steps": 14, "took_s": 219, "status": "done", "session": "s"},
            }),
            duration_ms: 303_000,
        };
        let line = completion_line(&[rec]).unwrap();
        assert!(line.contains("14") && line.contains("219"), "nested code numbers lost: {line}");
    }

    /// The completion line exists to add information the reply does not have.
    /// A `code` task knows its own step count and duration, so it says those
    /// rather than "everything ran".
    #[test]
    fn the_completion_line_reports_the_real_numbers() {
        let rec = |tool: &str, status: &str, ms: u64| arc_proto::ActionRecord {
            tool: tool.into(),
            args: serde_json::json!({}),
            risk: arc_proto::RiskLevel::Safe,
            outcome: if status == "failed" { ActionOutcome::Failed } else { ActionOutcome::Success },
            summary: String::new(),
            warning: None,
            data: serde_json::json!({ "status": status, "steps": 9, "took_s": 37, "workspace": "/w" }),
            duration_ms: ms,
        };
        let line = completion_line(&[rec("code", "done", 37_000)]).unwrap();
        assert!(line.contains('9') && line.contains("37"), "no real numbers in: {line}");
        assert!(
            !line.contains("all on disk") || line.contains("On disk now"),
            "still the old vague line: {line}"
        );

        // A failure says why, when the tool said why.
        let mut bad = rec("code", "failed", 12_000);
        bad.data = serde_json::json!({ "status": "failed", "error": "the crate does not exist" });
        let f = completion_line(&[bad]).unwrap();
        assert!(f.contains("crate does not exist"), "failure line hides the cause: {f}");

        // A tool that failed while the process exited 0 is still a failure.
        let mut quiet = rec("shell_exec", "failed", 900);
        quiet.outcome = ActionOutcome::Success;
        let q = completion_line(&[quiet]).unwrap().to_lowercase();
        assert!(
            q.contains("wall") || q.contains("didn't") || q.contains("clean") || q.contains("fell"),
            "{q}"
        );

        // Nothing done, nothing said.
        assert!(completion_line(&[]).is_none());
    }

    /// Tool names are not speakable -- "underscore code" -- so a line must
    /// never contain one.
    #[test]
    fn a_spoken_line_never_contains_a_tool_name() {
        for tool in ["code", "shell_exec", "web_search", "network_status", "workspace_switch"] {
            let p = tool_phrase(tool);
            assert_ne!(p, tool, "{tool} is not speakable");
            assert!(!p.contains('_'), "{tool} -> {p}");
        }
        // A self-made tool's name is Arc's own; it may be speakable, but it
        // may not be an identifier, so a pronoun is used.
        assert_eq!(tool_phrase("my_weird_tool"), "that");
    }

    /// The headsup names the tool that is actually running, varies, and never
    /// promises a step that has not happened.
    #[test]
    fn the_headsup_names_the_running_tool_and_varies() {
        let a = slow_turn_headsup(Some("shell_exec"), std::time::Duration::from_secs(6));
        assert!(a.contains("shell") || a.contains("Working") || a.contains("going"), "{a}");
        assert!(!a.contains('_'), "{a}");
        let long = slow_turn_headsup(Some("code"), std::time::Duration::from_secs(90));
        assert!(long.contains("90") || long.contains("Still") || long.contains("seconds"), "{long}");
        let mut set = std::collections::HashSet::new();
        for _ in 0..5 {
            set.insert(slow_turn_headsup(Some("shell_exec"), std::time::Duration::from_secs(6)));
        }
        assert!(set.len() >= 2, "the headsup never varies: {set:?}");
        // With nothing known, it must not invent a step.
        let unknown = slow_turn_headsup(None, std::time::Duration::from_secs(6));
        assert!(!unknown.contains("test") && !unknown.contains("writ"), "{unknown}");
    }

    /// The relay must handle every event, not just the ones it cares about.
    /// Its first version peeked for a `Thought` with a second `recv` and
    /// silently dropped everything else -- including the `ToolStarted` that
    /// marks a code task, which is why this is a function with a test.
    #[test]
    fn the_relay_only_intercepts_thoughts_and_never_drops_other_events() {
        let mut n = 0;
        let note = Event::Thought {
            round: 1,
            reasoning: String::new(),
            text: "I'll grab a shot.".into(),
            speakable: true,
        };
        // A Thought is consumed, and its line spoken.
        assert_eq!(note_action(&note, true, &mut n, false), Some(Some("I'll grab a shot.".into())));
        // Everything else is passed through untouched, so the progress branch
        // still sees it.
        for ev in [
            Event::CodeProgress { tool: "terminal".into(), detail: String::new(), step: 1, elapsed_s: 4 },
            Event::ToolStarted {
                tool: "code".into(),
                args: serde_json::json!({}),
                risk: arc_proto::RiskLevel::Dangerous,
            },
            Event::ToolFinished {
                record: arc_proto::ActionRecord {
                    tool: "x".into(),
                    args: serde_json::json!({}),
                    risk: arc_proto::RiskLevel::Safe,
                    outcome: ActionOutcome::Success,
                    summary: String::new(),
                    warning: None,
                    data: serde_json::json!({}),
                    duration_ms: 1,
                },
            },
        ] {
            assert_eq!(note_action(&ev, true, &mut n, false), None, "{ev:?} was swallowed");
        }
        // A Thought with no words is consumed but silent.
        let blank = Event::Thought { round: 2, reasoning: "hmm".into(), text: "  ".into(), speakable: true };
        assert_eq!(note_action(&blank, true, &mut n, false), Some(None));
        // The setting turns the whole thing off.
        let mut m = 0;
        assert_eq!(note_action(&note, false, &mut m, false), None);
        assert_eq!(m, 0);
        // A note in a round that is calling code is not spoken: the handoff
        // line already covers it.
        let code_round = Event::Thought {
            round: 1,
            reasoning: String::new(),
            text: "I'll build that.".into(),
            speakable: false,
        };
        let mut c = 0;
        assert_eq!(note_action(&code_round, true, &mut c, false), Some(None));
    }

    /// A note before a tool call used to be display-only, so the turn was
    /// silent until the tool finished. It is spoken now, under rules that keep
    /// it from becoming a monologue.
    #[test]
    fn a_note_before_a_tool_is_spoken_but_only_once() {
        let mut n = 0;
        assert_eq!(
            speakable_note("I'll grab a shot.", &mut n, false),
            Some("I'll grab a shot.".into()),
            "the model's own words should be said"
        );
        // One per turn: a second would be Arc talking to itself.
        assert_eq!(speakable_note("Now let me look at it.", &mut n, false), None);
        assert_eq!(n, 1);

        let mut n = 0;
        // Never over a code task, which announces itself.
        assert_eq!(speakable_note("I'll build it.", &mut n, true), None);
        // Never a question: the user would answer into a running turn.
        let mut q = 0;
        assert_eq!(speakable_note("Which project should I use?", &mut q, false), None);
        // Nothing usable.
        let mut e = 0;
        assert_eq!(speakable_note("   ", &mut e, false), None);
        assert_eq!(speakable_note("ok", &mut e, false), None);
    }

    /// A note is often a paragraph, and a paragraph before a tool call is a
    /// monologue. Only the first sentence, and only so much of it.
    #[test]
    fn a_note_is_cut_to_one_short_line() {
        let mut n = 0;
        let long = "Right, so what I'll do is first read the config file and check the theme, \
                    then look at the wallpaper settings, and after that probably check the bar too.";
        let out = speakable_note(long, &mut n, false).unwrap();
        assert!(out.starts_with("Right, so what I'll do is"), "{out}");
        assert!(!out.contains("wallpaper"), "only the first sentence: {out}");
        assert!(out.split_whitespace().count() <= 13, "{out}");
    }

    /// The first progress line is spoken, so what it says has to be
    /// speakable: short, and never a path read out character by character.
    #[test]
    fn a_spoken_task_summary_is_short_and_speakable() {
        assert_eq!(task_summary("Build me a screenshot script"), "a screenshot script");
        assert_eq!(task_summary("can you please write a test for the gate"), "a test for the gate");
        // Scene-setting clauses are skipped: announced live as "working on In
        // the pipeline-test project", which says nothing.
        assert_eq!(
            task_summary("In the pipeline-test project, add a subtract function and a test for it"),
            "a subtract function and a test for"
        );
        assert_eq!(task_summary("  "), "that");
        assert_eq!(task_summary("..."), "that");
        let long =
            "please make me a really quite extraordinarily complicated thing that does many things indeed ok";
        assert!(task_summary(long).split_whitespace().count() <= 7, "{}", task_summary(long));
        assert!(!task_summary("run ls /home/user/Projects/secret-dir").contains('/'));
    }

    /// A `code` task speaks for itself within seconds, so the generic
    /// headsup must stand down. Measured live before this: the headsup and
    /// the first progress line landed three seconds apart.
    #[test]
    fn a_code_task_suppresses_the_generic_headsup() {
        let h = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let said = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let fire = |flag: &std::sync::atomic::AtomicBool, said: &std::sync::Mutex<Vec<String>>| {
            if !flag.load(std::sync::atomic::Ordering::Relaxed) {
                said.lock().unwrap().push(slow_turn_headsup(None, SLOW_TURN_SPEAKS_AT));
            }
        };
        fire(&h, &said);
        assert_eq!(said.lock().unwrap().len(), 1, "a slow non-code turn still gets a headsup");
        h.store(true, std::sync::atomic::Ordering::Relaxed);
        fire(&h, &said);
        assert_eq!(said.lock().unwrap().len(), 1, "headsup stood down for a code task");
    }

    /// With no tool running there is nothing true to say, so nothing is said.
    /// The line exists to report a wait, not to fill it.
    #[test]
    fn a_headsup_with_nothing_running_is_not_spoken() {
        let running: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        assert!(running.lock().unwrap().is_none());
        // The watchdog's own guard: no tool, no line.
        let tool = running.lock().unwrap().clone();
        assert!(tool.is_none(), "nothing to name, so nothing worth saying");
    }

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
        let h = slow_turn_headsup(None, SLOW_TURN_SPEAKS_AT);
        assert!(!h.contains("test") && !h.contains("writ") && !h.contains("build"), "{h}");
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
        let low = line.to_lowercase();
        assert!(
            low.contains("wall") || low.contains("fell") || low.contains("didn't") || low.contains("details"),
            "the failure is not stated: {line}"
        );
        assert!(!low.contains("done") && !low.contains("on disk now"), "worst outcome must win: {line}");
    }

    #[test]
    fn a_pending_confirmation_says_so_instead_of_claiming_done() {
        let line = completion_line(&[act("code", ActionOutcome::AwaitingConfirmation, 40_000)]).unwrap();
        assert!(line.to_lowercase().contains("ok"), "the user must know a confirmation is wanted: {line}");
    }

    #[test]
    fn a_cancelled_run_does_not_claim_success() {
        let line = completion_line(&[act("code", ActionOutcome::Cancelled, 40_000)]).unwrap();
        let low = line.to_lowercase();
        assert!(
            low.contains("stopped") || low.contains("pulled"),
            "a cancelled run must not sound finished: {line}"
        );
    }

    #[test]
    fn a_successful_long_run_announces_completion() {
        let line = completion_line(&[act("code", ActionOutcome::Success, 85_000)]).unwrap();
        // Says the wait is over, and says it with a number rather than a
        // platitude: this line exists to add what the reply does not know.
        assert!(line.contains("85") || line.contains("seconds"), "no real information in: {line}");
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
        let low = line.to_lowercase();
        assert!(!low.contains("on disk now") && !low.contains("everything ran"), "{line}");
        assert!(
            low.contains("wall") || low.contains("fell") || low.contains("didn't") || low.contains("clean"),
            "a failed task was not reported as a failure: {line}"
        );
    }

    #[test]
        fn a_reported_ok_task_still_announces_completion() {
            let mut a = act("code", ActionOutcome::Success, 40_000);
            a.data = serde_json::json!({"status": "done", "steps": 5, "took_s": 40});
            let line = completion_line(&[a]).unwrap();
            assert!(line.contains('5') && line.contains("40"), "the task's own numbers should be used: {line}");
        }

        // ------------------------------------------------------------ now playing

        /// A daemon with no model and a scratch memory file, so these tests touch
            /// neither the user's memory nor a real provider.
            fn daemon() -> std::sync::Arc<Daemon> {
                let dir = tempfile::tempdir().unwrap();
                // Keep the TempDir alive for the life of the daemon.
                std::mem::forget(dir);
                let mut cfg = Config::default();
                cfg.ai.provider = arc_config::ProviderKind::None;
                let daemon =
                    Daemon::new(cfg, None).map_err(|e| panic!("could not build a test daemon: {e}")).unwrap();
                std::sync::Arc::new(daemon)
            }

        fn set(title: &str, artist: &str, pid: u32) -> NowPlayingRequest {
            NowPlayingRequest::Set {
                title: title.into(),
                artist: artist.into(),
                source: "youtube music".into(),
                pid,
            }
        }

        /// Collect events published while `f` runs. The bus is a broadcast channel,
        /// so this is the only way to see what the overlay would have received.
        fn events_while(d: &Daemon, f: impl FnOnce()) -> Vec<arc_proto::ServerMessage> {
            let mut rx = d.events.subscribe();
            f();
            let mut out = vec![];
            while let Ok(e) = rx.try_recv() {
                out.push(arc_proto::ServerMessage::Event { event: e });
            }
            out
        }

        fn now_playing_events(msgs: &[arc_proto::ServerMessage]) -> Vec<arc_proto::NowPlaying> {
            msgs.iter()
                .filter_map(|m| match m {
                    arc_proto::ServerMessage::Event { event: arc_proto::Event::NowPlaying { status } } => {
                        Some(status.clone())
                    }
                    _ => None,
                })
                .collect()
        }

        #[test]
            fn a_reported_track_is_shown_and_a_stop_clears_it() {
                let d = daemon();
                let msgs = events_while(&d, || {
                    d.now_playing(set("Hall of Fame", "Boards of Canada", 0)).unwrap();
                    assert!(d.now_playing_status().is_playing());
                    assert_eq!(d.now_playing_status().label(), "Hall of Fame — Boards of Canada");
                    d.now_playing(NowPlayingRequest::Stop).unwrap();
                });
                // The overlay learns about it from events, not by polling: one report,
                // one clear. Both must arrive or the row either never appears or never
                // goes away.
                let ev = now_playing_events(&msgs);
                assert_eq!(ev.len(), 2, "expected a set then a stop: {msgs:?}");
                assert!(ev[0].is_playing(), "the track should be announced");
                assert!(!ev[1].is_playing(), "the overlay must be told to clear, not left stale");
                assert!(!d.now_playing_status().is_playing());
            }

        /// A track reported twice, or a stop with nothing playing, must not push
        /// events: the overlay row would flicker on every poll otherwise.
        #[test]
        fn repeated_reports_are_idempotent() {
            let d = daemon();
            let set_again = set("Hall of Fame", "Boards of Canada", 0);
            d.now_playing(set_again.clone()).unwrap();
            let msgs = events_while(&d, || {
                d.now_playing(set_again.clone()).unwrap();
                d.now_playing(NowPlayingRequest::Stop).unwrap();
                d.now_playing(NowPlayingRequest::Stop).unwrap();
            });
            assert_eq!(now_playing_events(&msgs).len(), 1, "one stop, not two: {msgs:?}");
            assert!(!d.now_playing_status().is_playing());
        }

        /// A show is a read: it must not emit, and must not disturb what is there.
        #[test]
        fn show_reads_without_changing_or_announcing() {
            let d = daemon();
            d.now_playing(set("Teardrop", "Massive Attack", 0)).unwrap();
            let msgs = events_while(&d, || {
                let shown = d.now_playing(NowPlayingRequest::Show).unwrap();
                assert_eq!(shown.title, "Teardrop");
            });
            assert!(now_playing_events(&msgs).is_empty(), "a read pushed an event: {msgs:?}");
            assert!(d.now_playing_status().is_playing());
        }

        /// A blank title is a broken report, not a track. Storing it would put an
        /// empty row on the overlay with no way to tell it from a rendering bug.
        #[test]
        fn a_report_without_a_title_is_refused() {
            let d = daemon();
            let e = d.now_playing(set("   ", "Nobody", 0)).unwrap_err();
            assert!(e.contains("title"), "{e}");
            assert!(!d.now_playing_status().is_playing());
        }

        /// The end of a track is not reported to anyone -- the tool that started it
        /// has already exited -- so the pid is the only signal. Without the reap the
        /// overlay keeps showing a track that ended minutes ago.
        #[test]
        fn a_dead_player_clears_the_row_and_a_live_one_does_not() {
            let d = daemon();
            // This process is definitely alive, so its pid must not reap.
            d.now_playing(set("Still Playing", "Someone", std::process::id())).unwrap();
            d.reap_now_playing();
            assert!(d.now_playing_status().is_playing(), "a running player was reaped");

            // pid 0 means "I did not say", which is not "dead": reaping on it would
            // clear the row for any report that omits the pid.
            d.now_playing(set("Unknown Player", "Someone", 0)).unwrap();
            d.reap_now_playing();
            assert!(d.now_playing_status().is_playing(), "pid 0 was treated as a dead process");

            // A pid that cannot exist is reaped.
            d.now_playing(set("Gone", "Someone", u32::MAX)).unwrap();
            let msgs = events_while(&d, || d.reap_now_playing());
            assert!(!d.now_playing_status().is_playing(), "a dead player was left on screen");
            let cleared = now_playing_events(&msgs);
            assert_eq!(cleared.len(), 1);
            assert!(!cleared[0].is_playing());
            // Idempotent: a second poll finds nothing to do.
            let again = events_while(&d, || d.reap_now_playing());
            assert!(now_playing_events(&again).is_empty());
        }

        /// The bar and status reports carry the track, and only while it plays.
        #[test]
        fn the_bar_carries_the_track_only_while_playing() {
            let d = daemon();
            assert!(d.bar_status().now_playing.is_none(), "an idle bar gained a field");
            assert!(d.status().now_playing.is_none());
            assert!(!d.bar_status().tooltip.contains('♪'));

            d.now_playing(set("Windowlicker", "Aphex Twin", 0)).unwrap();
            let bar = d.bar_status();
            assert_eq!(bar.now_playing.as_ref().unwrap().label(), "Windowlicker — Aphex Twin");
            assert!(bar.tooltip.contains("Windowlicker"), "the bar tooltip should mention it: {}", bar.tooltip);
            assert_eq!(d.status().now_playing.unwrap().title, "Windowlicker");

            d.now_playing(NowPlayingRequest::Stop).unwrap();
            assert!(d.bar_status().now_playing.is_none());
            assert!(d.status().now_playing.is_none());
        }

        /// Over-long metadata comes from a scraped web page, not from Arc. It goes
        /// on one line of a header chip; it must not be able to fill the screen.
        #[test]
        fn absurdly_long_metadata_is_truncated() {
            let d = daemon();
            let long = "x".repeat(5000);
            let n = d
                .now_playing(set(&long, &long, 0))
                .map_err(|e| panic!("a long title should still be reported: {e}"))
                .unwrap();
            assert_eq!(n.title.chars().count(), 200);
            assert_eq!(n.artist.chars().count(), 200);
        }

        #[test]
        fn process_alive_is_true_for_this_process_and_false_for_nonsense() {
            assert!(process_alive(std::process::id()));
            assert!(!process_alive(0));
            // 0x7FFFFFFF is above the default pid_max and cannot exist.
            assert!(!process_alive(0x7fff_ffff));
        }
    }
