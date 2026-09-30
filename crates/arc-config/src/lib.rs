//! Arc configuration.
//!
//! * Schema + defaults: [`Config`] (every field has a default, so a partial
//!   file — or no file — is valid).
//! * Loading: [`load`] parses `~/.config/arc/config.toml`, reports unknown keys
//!   as warnings (typos never silently disable a feature) and validates ranges.
//! * Editing: [`set_value`] edits one key in place with `toml_edit`, keeping
//!   the user's comments and formatting, and refuses edits that fail validation.
//! * Automations live in a separate file, see [`automations`].

pub mod automations;
pub mod classification;
pub mod paths;

use arc_proto::{RiskLevel, VoiceMode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The documented default configuration shipped with Arc. `install.sh`
/// copies it to `~/.config/arc/config.toml` when no config exists.
pub const DEFAULT_CONFIG_TOML: &str = include_str!("../../../config/config.toml");

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("parsing {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("unknown configuration key `{0}`")]
    UnknownKey(String),
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// serde default for a bool that should be on unless configured off.
fn yes_default() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub personality: Personality,
    pub ai: Ai,
    pub voice: Voice,
    pub ui: Ui,
    pub bar: Bar,
    pub permissions: Permissions,
    pub apps: Apps,
    pub files: Files,
    pub web: Web,
    pub memory: Memory,
    pub logging: Logging,
    pub tools: Tools,
    pub code: Code,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct General {
    /// Assistant name used in replies and the UI.
    pub name: String,
    /// Names the assistant answers to at the start of an utterance, as the
    /// speech recogniser tends to spell them ("arc", "ark").
    pub name_variants: Vec<String>,
}

impl Default for General {
    fn default() -> Self {
        Self { name: "Arc".into(), name_variants: vec!["arc".into(), "ark".into()] }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Personality {
    /// Extra instructions appended to the language-model system prompt.
    pub custom_prompt: String,
    /// How Arc addresses the user ("" = no form of address).
    pub user_title: String,
}

impl Default for Personality {
    fn default() -> Self {
        Self {
            // Arc has a personality by default, not an empty slot. It used
            // to be empty so the shipped config and Config::default() agreed;
            // now the shipped config carries the real voice and the default
            // follows it, so a fresh install sounds like Arc too.
            custom_prompt: r#"Relaxed, confident, a bit wry. You're a very capable friend who happens to live on this machine, not a corporate assistant. Use contractions: "yeah", "yep", "nope", "I'll", "that's". Never say "Certainly", "I would be happy to", or "The requested operation has completed". Say "Done." or "Yeah, give me a second." and mean it.

Humor when it fits, never forced: dry observations, mild sarcasm, the occasional joke. Don't quip on every line -- a joke in every reply is just noise. Match the user's mood. If they're frustrated, acknowledge it plainly and move on to the fix. If something works first try, you can note your mild disappointment.

Be short for simple actions ("Done.", "Opening Firefox."), and go longer when there's actually something to say: a real explanation, a useful connection, a couple of options. Don't pad to look smart.

Never narrate step by step. Don't announce "I will now open the terminal" before doing it. Do the thing, then say what happened: "Found it, the dependency was outdated. Updated, tests pass."

When something fails, say what happened, what you think caused it, and what you're doing next -- in that order, briefly. When you're unsure, say so casually ("not sure yet, let me check") instead of hedging into mush. Never invent a fact to sound confident, and never claim an action happened unless a tool confirmed it.

Refer to things by context: "the project", "that window", "the other branch" should resolve from the conversation. Ask only when guessing would be worse than asking.

Never fake feelings, never stack on emojis, never be sarcastic at the user's expense. Say one short line before anything slow, then do it -- silence reads as broken.
"#.to_string().to_string(),
            user_title: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI or any OpenAI-compatible endpoint (OpenRouter, Groq, Ollama...).
    Openai,
    /// Anthropic Messages API.
    Anthropic,
    /// Hermes's local OpenAI-compatible proxy (`hermes proxy start`), which
    /// forwards to whichever provider the user is signed in to. Separate from
    /// `Openai` so the proxy can be the fallback while `openai` keeps serving
    /// an API-key provider for phrasing.
    Hermes,
    /// No language model: deterministic commands only.
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Ai {
    pub provider: ProviderKind,
    /// Provider to try when the primary one fails ("none" = no fallback).
    pub fallback: ProviderKind,
    pub temperature: f32,
    pub max_tokens: u32,
    pub timeout_s: u64,
    /// Maximum model→tool→model rounds per request.
    pub max_tool_rounds: u32,
    /// Model used only to word the final spoken reply. "none" (the default)
    /// means the tool-calling model also writes the reply, as before.
    pub phrasing: ProviderKind,
    /// Rewrites the final reply before it is spoken. Off means replies are
    /// passed through untouched, which costs nothing.
    pub phrasing_enabled: bool,
    /// Design and register a tool when the model asks for something nothing it
    /// has can do. Off means a missing capability is reported instead of built.
    pub auto_create_tools: bool,
    /// Tools auto-created in one request. 1 keeps a wrong guess from turning
    /// into a directory of half-tools.
    pub auto_create_max_per_turn: u32,
    /// Tools auto-created before auto-creation stops for the session.
    pub auto_create_max_per_session: u32,
    pub openai: RemoteAi,
    pub anthropic: RemoteAi,
    /// Endpoint for the Hermes proxy fallback (`hermes proxy start`).
    pub hermes: RemoteAi,
}

impl Default for Ai {
    fn default() -> Self {
        Self {
            provider: ProviderKind::Hermes,
            // Nothing to fall back to: the local model was removed, so a
            // proxy outage means no answer rather than a worse one.
            fallback: ProviderKind::None,
            // 0.6 measured no tool-calling cost against the hermes model,
            // where 0.0 read as flat. Warmth belongs in the personality.
            temperature: 0.6,
            // Capped for SPEECH rather than for the model. Kokoro synthesises
            // at real-time factor 1.0, so a token is roughly a second the user
            // waits and cannot barge in. Measured over 7 questions: 600 gave a
            // median 85s reply, 400 gave 73s with 0/7 truncated, and 300 cut
            // one answer off mid-sentence.
            max_tokens: 400,
            // Cloud round-trips are slower than localhost.
            timeout_s: 120,
            max_tool_rounds: 4,
            // The primary already speaks well, so the same connection does
            // the phrasing. Naming any other provider here would add a second
            // one and a second round trip for no measured gain.
            phrasing: ProviderKind::Hermes,
            phrasing_enabled: true,
            // Arc writing its own tools is the whole point of tool_create, and
            // creating one grants nothing: a composite adds no capability and
            // a script asks before every run. So the implicit path is on by
            // default, one tool per request and five per session, with the
            // destructive screen in `arc_core::autocreate` in front of it.
            auto_create_tools: true,
            auto_create_max_per_turn: 1,
            auto_create_max_per_session: 5,
            // The proxy ignores the bearer: `hermes proxy start` attaches the
            // user's own Portal credentials. Any non-empty value works.
            hermes: RemoteAi {
                base_url: "http://127.0.0.1:8645/v1".into(),
                model: "stealth/space-bunny-alpha".into(),
                api_key_env: "ARC_HERMES_PROXY_KEY".into(),
            },
            openai: RemoteAi {
                base_url: "https://api.openai.com/v1".into(),
                model: "gpt-4.1-mini".into(),
                api_key_env: "OPENAI_API_KEY".into(),
            },
            anthropic: RemoteAi {
                base_url: "https://api.anthropic.com".into(),
                model: "claude-sonnet-4-5".into(),
                api_key_env: "ANTHROPIC_API_KEY".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct RemoteAi {
    pub base_url: String,
    pub model: String,
    /// Name of the environment variable holding the API key. Keys are never
    /// stored in config.toml; put them in the environment or in
    /// ~/.config/arc/secrets.env (mode 0600).
    pub api_key_env: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SttEngine {
    Moonshine,
    Whisper,
    Voxtype,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TtsEngine {
    Piper,
    /// Kokoro-82M: much more natural prosody; several voices per model
    /// (`tts_speaker`). Needs ~0.5x real time on a 6-core CPU.
    Kokoro,
    Espeak,
    None,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WakeEngine {
    /// Transcribe each detected speech segment and look for the wake phrase
    /// at its start. Accurate, costs CPU only while someone is speaking.
    Stt,
    /// Streaming keyword spotter (lower CPU, less accurate for short names).
    Kws,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Voice {
    pub enabled: bool,
    pub mode: VoiceMode,
    /// Wake phrases (lowercase). A bare name from `general.name_variants`
    /// also works at the start of an utterance.
    pub wake_words: Vec<String>,
    pub wake_engine: WakeEngine,
    /// PipeWire node name, or "default".
    pub input_device: String,
    pub output_device: String,
    /// Speech rate multiplier (0.5 - 2.0).
    pub speech_rate: f32,
    /// Speech volume (0.0 - 1.0).
    pub volume: f32,
    /// Speak replies to voice requests.
    pub speak_replies: bool,
    /// Speak the words the model writes *alongside* a tool call ("I'll grab a
    /// shot."). Off by default before: the note was display-only, so a turn
    /// that opened with one was silent until the tool finished -- measured
    /// live at 135s with nothing said at all.
    #[serde(default = "yes_default")]
    pub speak_tool_notes: bool,
    /// Also speak replies to typed requests.
    pub speak_text_replies: bool,
    /// Short tones when listening starts/stops.
    pub chime: bool,
    /// Hard limit for one utterance.
    pub max_utterance_s: f32,
    /// Trailing silence that ends an utterance.
    pub end_silence_ms: u32,
    /// Give up if nothing is said within this time after listening starts.
    pub no_speech_timeout_s: f32,
    /// Silero VAD threshold (0-1).
    pub vad_threshold: f32,
    /// Keyword-spotter threshold (wake_engine = "kws").
    pub kws_threshold: f32,
    pub stt_engine: SttEngine,
    /// Model directory under ~/.local/share/arc/models (or absolute).
    pub stt_model: String,
    pub tts_engine: TtsEngine,
    pub tts_voice: String,
    /// Speaker within a multi-voice model (Kokoro): a name such as
    /// "bf_emma" or a numeric id. Empty = the model's first voice.
    pub tts_speaker: String,
    /// Allow the wake word to interrupt Arc while it is speaking.
    pub barge_in: bool,
    /// Keep models loaded (faster first response, ~300 MB RAM).
    pub preload: bool,
    /// Listen for a follow-up after Arc asks a question.
    pub follow_up: bool,
    /// Voice may confirm dangerous actions ("confirm"). Off by default: a
    /// dangerous action requested by voice is confirmed in the Arc panel or
    /// with `arc confirm`.
    pub voice_confirm_dangerous: bool,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: VoiceMode::WakeWord,
            wake_words: vec!["hey arc".into(), "okay arc".into(), "ok arc".into()],
            wake_engine: WakeEngine::Stt,
            input_device: "default".into(),
            output_device: "default".into(),
            speech_rate: 1.0,
            volume: 0.8,
            speak_replies: true,
            speak_tool_notes: true,
            speak_text_replies: false,
            chime: true,
            max_utterance_s: 15.0,
            end_silence_ms: 700,
            no_speech_timeout_s: 6.0,
            vad_threshold: 0.5,
            kws_threshold: 0.25,
            stt_engine: SttEngine::Moonshine,
            stt_model: "sherpa-onnx-moonshine-base-en-quantized-2026-02-27".into(),
            tts_engine: TtsEngine::Kokoro,
            tts_voice: "kokoro-en-v0_19".into(),
            tts_speaker: "af_bella".into(),
            barge_in: true,
            preload: true,
            follow_up: true,
            voice_confirm_dangerous: false,
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UiPosition {
    Top,
    Center,
    Bottom,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Ui {
    pub position: UiPosition,
    pub width: u32,
}

impl Default for Ui {
    fn default() -> Self {
        Self { position: UiPosition::Top, width: 560 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Bar {
    /// Write status for the Omarchy bar widget / Waybar module.
    pub enabled: bool,
}

impl Default for Bar {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicy {
    /// Run without confirmation (still logged).
    Allow,
    /// Always ask first.
    Confirm,
    /// Never run.
    Deny,
}

/// Hands a coding task to Hermes as a subprocess (`hermes chat -q`).
///
/// Arc does not write code itself. The model here calls one tool, that tool
/// spawns Hermes in a bounded working directory, and Hermes does the work with
/// the same tool access this agent has. The reason it is a subprocess and not
/// a prompt on the local model: a 2B asked to write a whole project produces
/// plausible-looking files that do not compile, and it has no way to iterate
/// on a build failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Code {
    /// Master switch. Off means the `code` tool refuses to run.
    pub enabled: bool,
    /// Hermes executable. `hermes` is resolved on PATH.
    pub binary: String,
    /// The only directory tree a task may touch. Tasks are told to stay here
    /// and the working directory is pinned to it.
    pub workspace: String,
    /// Hard wall-clock limit for one task, in seconds. Hermes is given a
    /// matching instruction and Arc kills the process at the limit.
    ///
    /// `0` means no limit: Arc does not wrap the task in a timeout at all, and
    /// says so in the prompt rather than promising a deadline it will not
    /// enforce. A build that legitimately needs twenty minutes was being
    /// killed at fifteen with nothing on disk to show for it.
    pub timeout_s: u64,
    /// Cap on captured output, so a runaway build cannot flood the prompt.
    pub max_output_bytes: usize,
    /// Whether to ask before each task.
    ///
    /// Off means Arc writes files and runs builds on a spoken instruction with
    /// no prompt. That is the point of a hands-free coding assistant, and it
    /// is only as safe as `screen()`, which is the sole remaining gate -- so
    /// the screen matches implied destructive intent, not just literal
    /// commands. Kept as a setting rather than deleted so it can be turned
    /// back on without a rebuild.
    pub confirm: bool,
    /// Reasoning effort passed to Hermes for a task.
    pub reasoning: String,
    /// Wall-clock budget handed to Hermes as --run-budget. Same ceiling as
    /// `timeout_s`, but stated to the agent as well as enforced by Arc, so it
    /// can wrap up rather than be killed mid-write.
    ///
    /// `0` omits the flag entirely. Hermes documents its own budget as
    /// "Unset = off"; passing `0` is not the same thing and is not treated as
    /// such here.
    pub run_budget_s: u64,
}

impl Default for Code {
    fn default() -> Self {
        Self {
            enabled: false,
            binary: "hermes".into(),
            workspace: "~/Projects".into(),
            // No limit by default. Arc is hands-free: a fifteen-minute
            // ceiling on a build was arbitrary, and being killed mid-write
            // leaves the user with nothing. Set these to a number to restore
            // a ceiling.
            timeout_s: 0,
            max_output_bytes: 16384,
            confirm: true,
            reasoning: "medium".into(),
            run_budget_s: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Permissions {
    /// Lowest risk level that requires explicit confirmation.
    pub confirm_at: RiskLevel,
    /// Seconds a pending confirmation stays valid.
    pub confirmation_timeout_s: u64,
    /// Per-tool overrides, e.g. `"files.move" = "confirm"`.
    pub tools: BTreeMap<String, ToolPolicy>,
    pub shell: ShellPolicy,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            confirm_at: RiskLevel::Dangerous,
            confirmation_timeout_s: 90,
            tools: BTreeMap::new(),
            shell: ShellPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ShellPolicy {
    /// Allow the `shell.run` / `terminal.run` tools at all.
    pub enabled: bool,
    /// Commands the language model may run without confirmation, as regexes
    /// matched against the whole command line (in addition to Arc's built-in
    /// read-only allowlist).
    pub allow: Vec<String>,
    /// Regexes that are always refused (in addition to Arc's built-in
    /// denylist: filesystem wipes, disk formatting, fork bombs...).
    pub deny: Vec<String>,
    /// Treat every command not on an allowlist as needing confirmation.
    pub confirm_unlisted: bool,
    pub timeout_s: u64,
    /// Max bytes of output returned to Arc.
    pub max_output_bytes: usize,
}

impl Default for ShellPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            allow: vec![],
            deny: vec![],
            confirm_unlisted: true,
            timeout_s: 20,
            max_output_bytes: 16 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Apps {
    /// Spoken name → desktop id, executable, or role ("browser", "terminal",
    /// "editor", "files"). Roles resolve through the system defaults.
    pub aliases: BTreeMap<String, String>,
}

impl Default for Apps {
    fn default() -> Self {
        let mut aliases = BTreeMap::new();
        aliases.insert("browser".into(), "role:browser".into());
        aliases.insert("web browser".into(), "role:browser".into());
        aliases.insert("terminal".into(), "role:terminal".into());
        aliases.insert("editor".into(), "role:editor".into());
        aliases.insert("file manager".into(), "role:files".into());
        aliases.insert("files".into(), "role:files".into());
        aliases.insert("vs code".into(), "code".into());
        aliases.insert("vscode".into(), "code".into());
        aliases.insert("system monitor".into(), "btop".into());
        aliases.insert("task manager".into(), "btop".into());
        Self { aliases }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Files {
    /// File tools may only touch paths under these roots.
    pub allowed_roots: Vec<String>,
    /// Never read, list, move or open these (also hidden from search results).
    pub sensitive_paths: Vec<String>,
}

impl Default for Files {
    fn default() -> Self {
        Self {
            allowed_roots: vec!["~".into(), "/tmp".into(), "/run/media".into(), "/mnt".into()],
            sensitive_paths: vec![
                "~/.ssh".into(),
                "~/.gnupg".into(),
                "~/.password-store".into(),
                "~/.config/arc/secrets.env".into(),
                "~/.local/share/keyrings".into(),
                "~/.mozilla".into(),
                "~/.config/chromium".into(),
                "~/.config/google-chrome".into(),
                "~/.aws".into(),
                "~/.kube".into(),
                "~/.docker/config.json".into(),
                "~/.netrc".into(),
                "~/.hermes/.env".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Web {
    /// Search URL opened in the browser; `{query}` is replaced (URL-encoded).
    pub search_url: String,
}

impl Default for Web {
    fn default() -> Self {
        Self { search_url: "https://duckduckgo.com/?q={query}".into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Memory {
    pub enabled: bool,
}

impl Default for Memory {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Logging {
    /// error | warn | info | debug | trace (or a tracing filter string).
    pub level: String,
    // log_ai_payloads removed: it was deserialized and warned about at
    // startup, but no provider ever read it, so it could not actually log a
    // prompt. A switch that does nothing is worse than no switch.
}

impl Default for Logging {
    fn default() -> Self {
        Self { level: "info".into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Tools {
    /// Tools to disable entirely (exact names or `category.*`).
    pub disabled: Vec<String>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Result of loading configuration.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub path: PathBuf,
    /// True when no file existed and defaults are in use.
    pub defaulted: bool,
    /// Non-fatal problems (unknown keys, clamped values).
    pub warnings: Vec<String>,
}

/// Load the configuration from the default location.
pub fn load() -> Result<Loaded, ConfigError> {
    load_from(&paths::config_file())
}

pub fn load_from(path: &Path) -> Result<Loaded, ConfigError> {
    if !path.exists() {
        return Ok(Loaded {
            config: Config::default(),
            path: path.to_path_buf(),
            defaulted: true,
            warnings: vec![],
        });
    }
    let text = std::fs::read_to_string(path)
        .map_err(|source| ConfigError::Io { path: path.to_path_buf(), source })?;
    let (config, warnings) = parse_str(&text).map_err(|e| match e {
        ConfigError::Parse { message, .. } => ConfigError::Parse { path: path.to_path_buf(), message },
        other => other,
    })?;
    Ok(Loaded { config, path: path.to_path_buf(), defaulted: false, warnings })
}

/// Parse and validate config text. Returns the config plus warnings.
pub fn parse_str(text: &str) -> Result<(Config, Vec<String>), ConfigError> {
    let raw: toml::Table = toml::from_str(text)
        .map_err(|e| ConfigError::Parse { path: PathBuf::new(), message: e.to_string() })?;
    let config: Config = toml::from_str(text)
        .map_err(|e| ConfigError::Parse { path: PathBuf::new(), message: e.to_string() })?;
    let mut warnings = unknown_keys(&raw);
    warnings.extend(validate(&config)?);
    Ok((config, warnings))
}

/// Tables whose keys are user-defined (not part of the schema).
const OPEN_TABLES: &[&str] = &["apps.aliases", "permissions.tools"];

fn unknown_keys(raw: &toml::Table) -> Vec<String> {
    let defaults = toml::Table::try_from(Config::default()).expect("default config serializes");
    let mut out = vec![];
    walk_unknown(raw, &defaults, "", &mut out);
    out
}

fn walk_unknown(user: &toml::Table, known: &toml::Table, prefix: &str, out: &mut Vec<String>) {
    for (k, v) in user {
        let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        match known.get(k) {
            None => out.push(format!("unknown key `{path}` (ignored)")),
            Some(toml::Value::Table(kt)) => {
                if OPEN_TABLES.contains(&path.as_str()) {
                    continue;
                }
                if let toml::Value::Table(ut) = v {
                    walk_unknown(ut, kt, &path, out);
                }
            }
            Some(_) => {}
        }
    }
}

/// Validate ranges. Hard errors for values that would break things; the
/// returned warnings cover questionable-but-usable settings.
pub fn validate(c: &Config) -> Result<Vec<String>, ConfigError> {
    let mut errors = vec![];
    let mut warnings = vec![];
    let mut range = |name: &str, v: f32, lo: f32, hi: f32| {
        if !(lo..=hi).contains(&v) || v.is_nan() {
            errors.push(format!("{name} = {v} is outside {lo}..={hi}"));
        }
    };
    range("voice.speech_rate", c.voice.speech_rate, 0.5, 2.0);
    range("voice.volume", c.voice.volume, 0.0, 1.0);
    range("voice.vad_threshold", c.voice.vad_threshold, 0.05, 0.95);
    range("voice.kws_threshold", c.voice.kws_threshold, 0.01, 0.99);
    range("voice.max_utterance_s", c.voice.max_utterance_s, 2.0, 60.0);
    range("voice.no_speech_timeout_s", c.voice.no_speech_timeout_s, 1.0, 30.0);
    range("ai.temperature", c.ai.temperature, 0.0, 2.0);
    if !(150..=5000).contains(&c.voice.end_silence_ms) {
        errors.push(format!("voice.end_silence_ms = {} is outside 150..=5000", c.voice.end_silence_ms));
    }
    if c.ai.max_tool_rounds == 0 || c.ai.max_tool_rounds > 10 {
        errors.push("ai.max_tool_rounds must be 1..=10".into());
    }
    if c.ai.max_tokens < 32 {
        errors.push("ai.max_tokens must be at least 32".into());
    }
    if c.ai.auto_create_max_per_turn > 3 {
        errors.push("ai.auto_create_max_per_turn must be 0..=3".into());
    }
    if c.ai.auto_create_max_per_session > 20 {
        errors.push("ai.auto_create_max_per_session must be 0..=20".into());
    }
    if c.general.name.trim().is_empty() {
        errors.push("general.name must not be empty".into());
    }
    if c.voice.mode == VoiceMode::WakeWord
        && c.voice.wake_words.is_empty()
        && c.general.name_variants.is_empty()
    {
        errors.push("voice.mode = \"wake_word\" needs voice.wake_words or general.name_variants".into());
    }
    if !c.web.search_url.contains("{query}") {
        errors.push("web.search_url must contain {query}".into());
    }
    for pat in c.permissions.shell.allow.iter().chain(c.permissions.shell.deny.iter()) {
        if let Err(e) = regex_syntax_ok(pat) {
            errors.push(format!("permissions.shell pattern {pat:?}: {e}"));
        }
    }
    if c.permissions.confirm_at == RiskLevel::Safe {
        warnings.push("permissions.confirm_at = \"safe\" asks before every action".into());
    }
    if c.files.allowed_roots.iter().any(|r| r.trim() == "/") {
        warnings.push("files.allowed_roots contains \"/\": file tools can reach the whole filesystem".into());
    }
    if errors.is_empty() { Ok(warnings) } else { Err(ConfigError::Invalid(errors.join("; "))) }
}

/// Minimal regex sanity check without pulling the regex crate into config:
/// balanced parentheses/brackets. Full compilation happens in arc-security.
fn regex_syntax_ok(p: &str) -> Result<(), String> {
    let mut depth_paren = 0i32;
    let mut in_class = false;
    let mut escaped = false;
    for ch in p.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '(' if !in_class => depth_paren += 1,
            ')' if !in_class => {
                depth_paren -= 1;
                if depth_paren < 0 {
                    return Err("unbalanced ')'".into());
                }
            }
            _ => {}
        }
    }
    if depth_paren != 0 {
        return Err("unbalanced '('".into());
    }
    if in_class {
        return Err("unterminated '['".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

/// Read one value by dotted key from the effective configuration.
pub fn get_value(config: &Config, key: &str) -> Option<toml::Value> {
    let table = toml::Table::try_from(config).ok()?;
    let mut cur = toml::Value::Table(table);
    for part in key.split('.') {
        cur = match cur {
            toml::Value::Table(mut t) => t.remove(part)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Set `key` (dotted) to `raw_value` in the config file at `path`, preserving
/// comments. `raw_value` is parsed as TOML (`true`, `3`, `"x"`, `["a"]`);
/// anything that doesn't parse is treated as a string. The edit is validated
/// before it is written; on failure the file is untouched.
pub fn set_value(path: &Path, key: &str, raw_value: &str) -> Result<Vec<String>, ConfigError> {
    let text = if path.exists() {
        std::fs::read_to_string(path)
            .map_err(|source| ConfigError::Io { path: path.to_path_buf(), source })?
    } else {
        DEFAULT_CONFIG_TOML.to_string()
    };
    let new_text = set_value_in_str(&text, key, raw_value)?;
    let (_, warnings) = parse_str(&new_text)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| ConfigError::Io { path: dir.to_path_buf(), source })?;
    }
    atomic_write(path, new_text.as_bytes())
        .map_err(|source| ConfigError::Io { path: path.to_path_buf(), source })?;
    Ok(warnings)
}

pub fn set_value_in_str(text: &str, key: &str, raw_value: &str) -> Result<String, ConfigError> {
    // Reject keys the schema doesn't know (typos would otherwise be ignored).
    let defaults = toml::Table::try_from(Config::default()).expect("default config serializes");
    let parts: Vec<&str> = key.split('.').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(ConfigError::UnknownKey(key.into()));
    }
    {
        let mut known = &defaults;
        let mut open = false;
        for (i, part) in parts.iter().enumerate() {
            let prefix = parts[..i].join(".");
            if OPEN_TABLES.contains(&prefix.as_str()) {
                open = true;
                break;
            }
            match known.get(*part) {
                Some(toml::Value::Table(t)) if i + 1 < parts.len() => known = t,
                Some(_) if i + 1 == parts.len() => {}
                _ => return Err(ConfigError::UnknownKey(key.into())),
            }
        }
        let _ = open;
    }

    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        ConfigError::Parse { path: PathBuf::new(), message: e.to_string() }
    })?;
    let value: toml_edit::Value = match format!("v = {raw_value}").parse::<toml_edit::DocumentMut>() {
        Ok(d) => d["v"].as_value().cloned().unwrap_or_else(|| raw_value.into()),
        Err(_) => raw_value.into(),
    };
    let mut item: &mut toml_edit::Item = doc.as_item_mut();
    for part in &parts[..parts.len() - 1] {
        let tbl = item
            .as_table_like_mut()
            .ok_or_else(|| ConfigError::Invalid(format!("`{key}`: parent is not a table")))?;
        if tbl.get(part).is_none() {
            tbl.insert(part, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        item = tbl.get_mut(part).expect("just inserted");
    }
    let last = parts[parts.len() - 1];
    let tbl = item
        .as_table_like_mut()
        .ok_or_else(|| ConfigError::Invalid(format!("`{key}`: parent is not a table")))?;
    match tbl.get_mut(last) {
        Some(existing) if existing.is_value() => {
            // Keep the existing decoration (inline comments).
            let decor = existing.as_value().map(|v| v.decor().clone());
            let mut v = value;
            if let Some(d) = decor {
                *v.decor_mut() = d;
            }
            *existing = toml_edit::Item::Value(v);
        }
        _ => {
            tbl.insert(last, toml_edit::Item::Value(value));
        }
    }
    Ok(doc.to_string())
}

/// Write via a temp file + rename so readers never see a partial file.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("arc"),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

/// Path of the optional secrets file (KEY=VALUE lines, must be mode 0600).
pub fn secrets_file() -> PathBuf {
    paths::config_dir().join("secrets.env")
}

/// Look up a secret: the environment first, then `secrets.env`. The file is
/// ignored (with an error) unless it is private to the user.
pub fn secret(name: &str) -> Result<Option<String>, String> {
    if name.is_empty() {
        return Ok(None);
    }
    if let Ok(v) = std::env::var(name)
        && !v.is_empty()
    {
        return Ok(Some(v));
    }
    read_secret_file(&secrets_file(), name)
}

pub fn read_secret_file(path: &Path, name: &str) -> Result<Option<String>, String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{} is readable by other users; run `chmod 600 {}` (ignoring it)",
            path.display(),
            path.display()
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == name
        {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            return Ok(if v.is_empty() { None } else { Some(v.to_string()) });
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_default_config_matches_code_defaults() {
        let (cfg, warnings) = parse_str(DEFAULT_CONFIG_TOML).expect("shipped config parses");
        assert!(warnings.is_empty(), "shipped config has warnings: {warnings:?}");
        assert_eq!(cfg, Config::default(), "config/config.toml drifted from Config::default()");
    }

    #[test]
    fn empty_config_is_default() {
        let (cfg, w) = parse_str("").unwrap();
        assert_eq!(cfg, Config::default());
        assert!(w.is_empty());
    }

    #[test]
    fn partial_config_merges_with_defaults() {
        let (cfg, _) = parse_str("[voice]\nmode = \"wake_word\"\nvolume = 0.5\n").unwrap();
        assert_eq!(cfg.voice.mode, VoiceMode::WakeWord);
        assert_eq!(cfg.voice.volume, 0.5);
        assert_eq!(cfg.voice.speech_rate, 1.0);
        assert_eq!(cfg.ai, Ai::default());
    }

    #[test]
    fn unknown_keys_warn_but_open_tables_do_not() {
        let (_, w) = parse_str(
            "[voice]\nvolum = 0.5\n[apps.aliases]\n\"my browser\" = \"chromium\"\n[permissions.tools]\n\"files.move\" = \"confirm\"\n[bogus]\nx=1\n",
        )
        .unwrap();
        assert!(w.iter().any(|m| m.contains("voice.volum")), "{w:?}");
        assert!(w.iter().any(|m| m.contains("`bogus`")), "{w:?}");
        assert!(!w.iter().any(|m| m.contains("aliases") || m.contains("permissions.tools")), "{w:?}");
    }

    #[test]
    fn invalid_ranges_are_errors() {
        assert!(parse_str("[voice]\nvolume = 1.5\n").is_err());
        assert!(parse_str("[voice]\nspeech_rate = 0.1\n").is_err());
        assert!(parse_str("[web]\nsearch_url = \"https://x\"\n").is_err());
        assert!(parse_str("[permissions.shell]\ndeny = [\"(unclosed\"]\n").is_err());
    }

    #[test]
    fn bad_enum_is_parse_error() {
        let e = parse_str("[ai]\nprovider = \"skynet\"\n").unwrap_err();
        assert!(matches!(e, ConfigError::Parse { .. }));
    }

    #[test]
    fn set_value_preserves_comments() {
        let src = "# top comment\n[voice]\n# the mode\nmode = \"push_to_talk\" # inline\nvolume = 0.8\n";
        let out = set_value_in_str(src, "voice.mode", "\"wake_word\"").unwrap();
        assert!(out.contains("# top comment"));
        assert!(out.contains("# the mode"));
        assert!(out.contains("mode = \"wake_word\""), "{out}");
        assert!(out.contains("# inline"), "{out}");
        let (cfg, _) = parse_str(&out).unwrap();
        assert_eq!(cfg.voice.mode, VoiceMode::WakeWord);
    }

    #[test]
    fn set_value_bare_string_and_new_table() {
        let out = set_value_in_str("", "ai.provider", "anthropic").unwrap();
        let (cfg, _) = parse_str(&out).unwrap();
        assert_eq!(cfg.ai.provider, ProviderKind::Anthropic);
        let out = set_value_in_str("", "apps.aliases.music", "\"spotify\"").unwrap();
        let (cfg, _) = parse_str(&out).unwrap();
        assert_eq!(cfg.apps.aliases.get("music").map(String::as_str), Some("spotify"));
    }

    #[test]
    fn set_value_rejects_unknown_keys() {
        assert!(matches!(set_value_in_str("", "voice.volum", "0.3"), Err(ConfigError::UnknownKey(_))));
        assert!(matches!(set_value_in_str("", "nope", "1"), Err(ConfigError::UnknownKey(_))));
    }

    #[test]
    fn set_value_file_validates_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[voice]\nvolume = 0.8\n").unwrap();
        assert!(set_value(&p, "voice.volume", "3.0").is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "[voice]\nvolume = 0.8\n");
        set_value(&p, "voice.volume", "0.4").unwrap();
        assert!(std::fs::read_to_string(&p).unwrap().contains("volume = 0.4"));
    }

    #[test]
    fn get_value_dotted() {
        let c = Config::default();
        assert_eq!(get_value(&c, "voice.volume"), Some(toml::Value::Float(0.8f32 as f64)));
        assert_eq!(get_value(&c, "voice.nope"), None);
    }

    #[test]
    fn secret_file_requires_private_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("secrets.env");
        std::fs::write(&p, "# c\nexport FOO_KEY=\"abc\"\nBAR=\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_secret_file(&p, "FOO_KEY").is_err());
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_secret_file(&p, "FOO_KEY").unwrap().as_deref(), Some("abc"));
        assert_eq!(read_secret_file(&p, "BAR").unwrap(), None);
        assert_eq!(read_secret_file(&p, "MISSING").unwrap(), None);
    }

    #[test]
    fn removed_config_keys_are_flagged_as_unknown() {
        // These were removed as dead: deserialized, documented, and never read
        // by anything. serde's `default` means a config can name them again and
        // still load, silently doing nothing, so assert the warning fires --
        // that is the only signal a reintroduced key produces.
        for (key, gone) in [
            ("ui.accent", "[ui]\naccent = \"#ff0000\"\n"),
            ("ui.opacity", "[ui]\nopacity = 0.9\n"),
            ("logging.log_ai_payloads", "[logging]\nlog_ai_payloads = true\n"),
        ] {
            let (_, warnings) = parse_str(gone).expect("unknown keys warn, they do not fail");
            assert!(
                warnings.iter().any(|w| w.contains(key)),
                "no warning for the removed key {key}: {warnings:?}"
            );
        }
    }
}
