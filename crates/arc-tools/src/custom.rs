//! Tools Arc makes for itself.
//!
//! Two kinds, both created by the model through `tool_create` without asking
//! (the user's choice), both persisted under `~/.local/share/arc/tools/<name>/`
//! and loaded again at start:
//!
//! * **composite** -- a named chain of existing tools, with `{param}`
//!   placeholders in the step arguments. It adds no capability: the gate runs
//!   every step as if it had been called on its own, so each is assessed,
//!   classified and confirmed exactly as that tool normally would be. The
//!   composite object here is only a name and a schema; see
//!   `arc_core::gate::Gate::run` for where the steps actually run.
//! * **script** -- a bash or python script the model wrote. That *is* new
//!   capability, arbitrary code nobody reviewed, so a script tool is
//!   dangerous by nature: every run asks, and the classification picker cannot
//!   lower it. Bash scripts are also screened line by line through the same
//!   analyzer `shell_exec` uses, and a line it blocks (`rm -rf /`, a fork
//!   bomb) refuses the whole script at creation *and* at every load.
//!
//! Deleting asks (the user's other choice); see `ToolDelete` in lib.rs.
//!
//! Parameters never touch the script text. They arrive as environment
//! variables (`ARC_ARG_<NAME>`) and as a JSON object on stdin, so a value like
//! `"; rm -rf ~"` is data to the script, not code.

use crate::{Json, JsonMap, Tool, ToolResult};
use arc_proto::RiskLevel;
use arc_security::shell::ShellAnalyzer;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Tool-management tools. A composite may not call these: a tool that can
/// create or delete tools would let one unconfirmed call manufacture others.
pub const MANAGEMENT: &[&str] =
    &["tool_create", "tool_delete", "tool_list_own", crate::TOOL_MISSING];

const MAX_STEPS: usize = 12;
const MAX_PARAMS: usize = 8;
const MAX_SCRIPT_BYTES: usize = 16 * 1024;
const MAX_OUTPUT_CHARS: usize = 4000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Bash,
    Python,
}

impl Language {
    fn file(self) -> &'static str {
        match self {
            Language::Bash => "run.sh",
            Language::Python => "run.py",
        }
    }
    fn interpreter(self) -> &'static str {
        match self {
            Language::Bash => "bash",
            Language::Python => "python3",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Step {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Json::is_null")]
    pub args: Json,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    Composite {
        steps: Vec<Step>,
    },
    Script {
        language: Language,
        /// Kept in its own file (run.sh / run.py) so it can be read and
        /// edited as a script, not as an escaped JSON string.
        #[serde(skip)]
        script: String,
    },
}

/// One self-made tool, as stored in `<dir>/<name>/tool.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Def {
    pub name: String,
    pub description: String,
    /// Parameter name -> description. Required for a composite (its steps
    /// need the value); optional for a script, which sees an unset
    /// `$ARC_ARG_<NAME>` and uses its own default.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    #[serde(default)]
    pub created: String,
    /// Hash of the script text as Arc wrote it. A script whose text no
    /// longer matches at load has been edited since, so any classification
    /// the user gave it -- having reviewed the *old* text -- is cleared.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fingerprint: String,
    #[serde(flatten)]
    pub body: Body,
}

/// FNV-1a, 64-bit. Detects an edited script; it is not a security boundary
/// (anyone who can write the file can write tool.json too, and the script
/// is re-screened against the blocklist at every load regardless).
pub fn fingerprint(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

impl Def {
    pub fn kind(&self) -> &'static str {
        match self.body {
            Body::Composite { .. } => "composite",
            Body::Script { .. } => "script",
        }
    }

    fn schema(&self) -> Json {
        let props: serde_json::Map<String, Json> =
            self.params.iter().map(|(k, d)| (k.clone(), serde_json::json!({ "description": d }))).collect();
        // Marking a script's parameters required forced the model to invent
        // a value for one it had meant as optional: measured live, a
        // screenshot tool with an optional `output` path was called with a
        // made-up path and date (shot_20260214_... on 2026-09-30) instead
        // of letting the script pick the default.
        let required: Vec<&String> = match self.body {
            Body::Composite { .. } => self.params.keys().collect(),
            Body::Script { .. } => vec![],
        };
        serde_json::json!({ "type": "object", "properties": props, "required": required })
    }
}

fn valid_name(n: &str, min: usize, max: usize) -> bool {
    let mut c = n.chars();
    matches!(c.next(), Some('a'..='z'))
        && n.len() >= min
        && n.len() <= max
        && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Every `{ident}` in a string.
fn placeholders(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        rest = &rest[i + 1..];
        if let Some(j) = rest.find('}') {
            let id = &rest[..j];
            if valid_name(id, 1, 31) {
                out.push(id.to_string());
            }
            rest = &rest[j + 1..];
        } else {
            break;
        }
    }
    out
}

fn strings_in(v: &Json, out: &mut Vec<String>) {
    match v {
        Json::String(s) => out.push(s.clone()),
        Json::Array(a) => a.iter().for_each(|x| strings_in(x, out)),
        Json::Object(m) => m.values().for_each(|x| strings_in(x, out)),
        _ => {}
    }
}

/// Substitute `{param}` placeholders. A string that is *only* a placeholder
/// takes the argument's JSON value as-is, so `{"id": "{workspace}"}` passes
/// the number 3 rather than the string "3" to a tool that wants a number.
pub fn fill(v: &Json, args: &JsonMap) -> Json {
    match v {
        Json::String(s) => {
            if let Some(id) = s.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
                if let Some(a) = args.get(id) {
                    return a.clone();
                }
            }
            let mut out = s.clone();
            for (k, a) in args {
                let text = match a {
                    Json::String(t) => t.clone(),
                    other => other.to_string(),
                };
                out = out.replace(&format!("{{{k}}}"), &text);
            }
            Json::String(out)
        }
        Json::Array(a) => Json::Array(a.iter().map(|x| fill(x, args)).collect()),
        Json::Object(m) => Json::Object(m.iter().map(|(k, x)| (k.clone(), fill(x, args))).collect()),
        other => other.clone(),
    }
}

/// The store every self-made tool lives in. Shared (behind an `Arc`) by the
/// registry, which looks tools up in it, and by the management tools, which
/// change it.
pub struct CustomTools {
    dir: RwLock<Option<PathBuf>>,
    /// Scripts whose text changed on disk since Arc created them, found at
    /// the last load. The daemon clears their classifications.
    changed: RwLock<Vec<String>>,
    defs: RwLock<BTreeMap<String, Def>>,
    builtins: RwLock<HashSet<String>>,
    analyzer: ShellAnalyzer,
    timeout_s: u64,
}

impl CustomTools {
    /// An empty, in-memory store. Nothing is read or written until
    /// [`CustomTools::attach_dir`] -- which only the daemon calls, so tests
    /// never see (or clobber) the user's real tools.
    pub fn new(analyzer: ShellAnalyzer, timeout_s: u64) -> Self {
        Self {
            dir: RwLock::new(None),
            changed: RwLock::new(vec![]),
            defs: RwLock::new(BTreeMap::new()),
            builtins: RwLock::new(HashSet::new()),
            analyzer,
            timeout_s: timeout_s.max(1),
        }
    }

    pub(crate) fn add_builtin(&self, name: &str) {
        self.builtins.write().unwrap().insert(name.to_string());
    }

    #[cfg(test)]
    fn set_builtins(&self, names: impl IntoIterator<Item = String>) {
        *self.builtins.write().unwrap() = names.into_iter().collect();
    }

    /// Point the store at a directory and load what is there. Returns one
    /// line per tool that was skipped, for the daemon to log. A tool that no
    /// longer passes validation (a bash line the analyzer now blocks, a name
    /// that became a built-in) is skipped, not loaded half-trusted.
    pub fn attach_dir(&self, dir: PathBuf) -> Vec<String> {
        let mut problems = vec![];
        let mut changed = vec![];
        let mut loaded = BTreeMap::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut paths: Vec<PathBuf> =
                entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            paths.sort();
            for p in paths {
                match self.read_one(&p) {
                    Ok(d) => {
                        if let Err(e) = self.validate(&d, &loaded) {
                            problems.push(format!("{}: {e}", p.display()));
                        } else {
                            if let Body::Script { script, .. } = &d.body {
                                if d.fingerprint.is_empty() {
                                    // Made before fingerprints existed. Such a
                                    // script could not have been lowered then
                                    // (scripts were locked at dangerous), so
                                    // there is no approval to protect: record
                                    // the text as it is now. Without this it
                                    // counted as edited on every start and
                                    // lost a new classification each restart.
                                    let mut d2 = d.clone();
                                    d2.fingerprint = fingerprint(script);
                                    if let Err(e) = self.write_def(&p, &d2) {
                                        problems.push(format!(
                                            "{}: could not record fingerprint: {e}",
                                            p.display()
                                        ));
                                    }
                                } else if fingerprint(script) != d.fingerprint {
                                    changed.push(d.name.clone());
                                }
                            }
                            loaded.insert(d.name.clone(), d);
                        }
                    }
                    Err(e) => problems.push(format!("{}: {e}", p.display())),
                }
            }
        }
        *self.defs.write().unwrap() = loaded;
        *self.dir.write().unwrap() = Some(dir);
        *self.changed.write().unwrap() = changed;
        problems
    }

    /// Scripts edited on disk since Arc created them (as of the last load).
    pub fn changed_since_created(&self) -> Vec<String> {
        self.changed.read().unwrap().clone()
    }

    fn read_one(&self, p: &Path) -> Result<Def, String> {
        let text = std::fs::read_to_string(p.join("tool.json")).map_err(|e| e.to_string())?;
        let mut d: Def = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if p.file_name().and_then(|n| n.to_str()) != Some(d.name.as_str()) {
            return Err(format!("directory name does not match tool name `{}`", d.name));
        }
        if let Body::Script { language, script } = &mut d.body {
            *script = std::fs::read_to_string(p.join(language.file())).map_err(|e| e.to_string())?;
        }
        Ok(d)
    }

    pub fn dir(&self) -> Option<PathBuf> {
        self.dir.read().unwrap().clone()
    }

    pub fn get(&self, name: &str) -> Option<Def> {
        self.defs.read().unwrap().get(name).cloned()
    }

    pub fn list(&self) -> Vec<Def> {
        self.defs.read().unwrap().values().cloned().collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.defs.read().unwrap().keys().cloned().collect()
    }

    /// Steps of a composite tool, for the gate to run one by one.
    pub fn composite_steps(&self, name: &str) -> Option<(Vec<Step>, BTreeMap<String, String>)> {
        match self.get(name)? {
            Def { body: Body::Composite { steps }, params, .. } => Some((steps, params)),
            _ => None,
        }
    }

    /// Composites whose steps call `name`.
    pub fn used_by(&self, name: &str) -> Vec<String> {
        self.defs
            .read()
            .unwrap()
            .values()
            .filter(|d| matches!(&d.body, Body::Composite { steps } if steps.iter().any(|s| s.tool == name)))
            .map(|d| d.name.clone())
            .collect()
    }

    /// The registry-facing tool object. `step_risk` gives a step tool's own
    /// level, so a composite reports the worst of its steps.
    pub fn tool(&self, name: &str, step_risk: &dyn Fn(&str) -> RiskLevel) -> Option<Arc<dyn Tool>> {
        let d = self.get(name)?;
        let path = self.dir().map(|dir| dir.join(&d.name));
        Some(match d.body {
            Body::Composite { ref steps } => {
                let risk = steps.iter().map(|s| step_risk(&s.tool)).max().unwrap_or(RiskLevel::Safe);
                Arc::new(CompositeTool { def: d, risk })
            }
            Body::Script { .. } => Arc::new(ScriptTool { def: d, path, timeout_s: self.timeout_s }),
        })
    }

    /// Everything that makes a definition acceptable. `existing` is the set
    /// it will join (so a composite may call a script tool made before it).
    fn validate(&self, d: &Def, existing: &BTreeMap<String, Def>) -> Result<(), String> {
        if !valid_name(&d.name, 3, 40) {
            return Err(format!(
                "`{}` is not a valid tool name: use 3-40 lowercase letters, digits and underscores, starting with a letter",
                d.name
            ));
        }
        if self.builtins.read().unwrap().contains(&d.name) || MANAGEMENT.contains(&d.name.as_str()) {
            return Err(format!("`{}` is a built-in tool; pick another name", d.name));
        }
        let desc = d.description.trim();
        if desc.is_empty() || desc.chars().count() > 300 {
            return Err(
                "description must be 1-300 characters: say what the tool does and when to use it".into()
            );
        }
        if d.params.len() > MAX_PARAMS {
            return Err(format!("at most {MAX_PARAMS} parameters"));
        }
        for p in d.params.keys() {
            if !valid_name(p, 1, 31) {
                return Err(format!("parameter `{p}` must be lowercase letters, digits and underscores"));
            }
        }
        match &d.body {
            Body::Composite { steps } => {
                if steps.is_empty() || steps.len() > MAX_STEPS {
                    return Err(format!("a composite needs 1-{MAX_STEPS} steps"));
                }
                let builtins = self.builtins.read().unwrap();
                for (i, s) in steps.iter().enumerate() {
                    let n = i + 1;
                    if MANAGEMENT.contains(&s.tool.as_str()) {
                        return Err(format!("step {n}: a tool cannot create, list or delete tools"));
                    }
                    if s.tool == d.name {
                        return Err(format!("step {n}: a tool cannot call itself"));
                    }
                    match existing.get(&s.tool) {
                        Some(Def { body: Body::Composite { .. }, .. }) => {
                            return Err(format!(
                                "step {n}: `{}` is itself a composite; list its steps here instead",
                                s.tool
                            ));
                        }
                        Some(_) => {}
                        None if builtins.contains(&s.tool) => {}
                        None => return Err(format!("step {n}: unknown tool `{}`", s.tool)),
                    }
                    if !(s.args.is_null() || s.args.is_object()) {
                        return Err(format!("step {n}: args must be an object"));
                    }
                    let mut strs = vec![];
                    strings_in(&s.args, &mut strs);
                    for id in strs.iter().flat_map(|x| placeholders(x)) {
                        if !d.params.contains_key(&id) {
                            return Err(format!(
                                "step {n} uses {{{id}}} but the tool has no parameter `{id}`"
                            ));
                        }
                    }
                }
            }
            Body::Script { language, script } => {
                // A shebang and comments are not a script. This is what a tool
                // call cut off by the token budget looks like, and it was
                // accepted live as a "screenshot" tool that did nothing.
                let body = script.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
                if body.count() == 0 {
                    return Err(
                        "the script has no commands (only a shebang or comments); send the whole script"
                            .into(),
                    );
                }
                if script.len() > MAX_SCRIPT_BYTES {
                    return Err(format!("the script is over {} KB", MAX_SCRIPT_BYTES / 1024));
                }
                if *language == Language::Bash {
                    for (i, line) in script.lines().enumerate() {
                        let l = line.trim();
                        if l.is_empty() || l.starts_with('#') {
                            continue;
                        }
                        if let Some(why) = self.analyzer.analyze(l).blocked {
                            return Err(format!("line {} is refused by the shell policy: {why}", i + 1));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Validate, persist (if a directory is attached), then register.
    /// Refuses to overwrite: replacing a tool loses the old one, and losing a
    /// tool is what the user asked to be consulted about.
    pub fn create(&self, mut d: Def) -> Result<Def, String> {
        d.description = d.description.trim().to_string();
        if self.get(&d.name).is_some() {
            return Err(format!(
                "a tool called `{}` already exists. Use it, or delete it first with tool_delete (the user will be asked)",
                d.name
            ));
        }
        let existing = self.defs.read().unwrap().clone();
        self.validate(&d, &existing)?;
        if d.created.is_empty() {
            d.created = chrono::Local::now().to_rfc3339();
        }
        if let Body::Script { script, .. } = &d.body {
            d.fingerprint = fingerprint(script);
        }
        if let Some(dir) = self.dir() {
            let p = dir.join(&d.name);
            std::fs::create_dir_all(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            if let Body::Script { language, script } = &d.body {
                let f = p.join(language.file());
                std::fs::write(&f, script).map_err(|e| format!("{}: {e}", f.display()))?;
            }
            self.write_def(&p, &d)?;
        }
        self.defs.write().unwrap().insert(d.name.clone(), d.clone());
        Ok(d)
    }

    /// Write `tool.json` (the metadata; a script's text lives beside it).
    fn write_def(&self, p: &Path, d: &Def) -> Result<(), String> {
        let json = serde_json::to_string_pretty(d).map_err(|e| e.to_string())?;
        std::fs::write(p.join("tool.json"), json + "\n").map_err(|e| e.to_string())
    }

    pub fn delete(&self, name: &str) -> Result<Def, String> {
        let d = self.get(name).ok_or_else(|| format!("no self-made tool called `{name}`"))?;
        let users = self.used_by(name);
        if !users.is_empty() {
            return Err(format!("`{name}` is used by {}; delete that first", users.join(", ")));
        }
        if let Some(dir) = self.dir() {
            // The name was validated at creation (no `/`, no `..`), so this
            // cannot leave the tools directory.
            let p = dir.join(&d.name);
            if p.parent() == Some(dir.as_path()) && p.exists() {
                std::fs::remove_dir_all(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            }
        }
        self.defs.write().unwrap().remove(name);
        Ok(d)
    }
}

/// A composite's face in the registry. Never executed directly: the gate
/// sees [`CustomTools::composite_steps`] and runs the steps itself.
struct CompositeTool {
    def: Def,
    risk: RiskLevel,
}

#[async_trait]
impl Tool for CompositeTool {
    fn name(&self) -> &str {
        &self.def.name
    }
    fn description(&self) -> &str {
        &self.def.description
    }
    fn parameters(&self) -> Json {
        self.def.schema()
    }
    fn base_risk(&self) -> RiskLevel {
        self.risk
    }
    /// A composite's level is only whether to hold the whole chain before
    /// it starts; every step is still gated on its own, so lowering one
    /// that contains a dangerous step changes nothing about that step.
    fn user_may_lower(&self) -> bool {
        true
    }
    async fn execute(&self, _args: &JsonMap) -> ToolResult {
        ToolResult::Error("a composite tool runs through the gate, step by step".into())
    }
}

struct ScriptTool {
    def: Def,
    path: Option<PathBuf>,
    timeout_s: u64,
}

#[async_trait]
impl Tool for ScriptTool {
    fn name(&self) -> &str {
        &self.def.name
    }
    fn description(&self) -> &str {
        &self.def.description
    }
    fn parameters(&self) -> Json {
        self.def.schema()
    }
    fn base_risk(&self) -> RiskLevel {
        RiskLevel::Dangerous
    }
    fn user_may_lower(&self) -> bool {
        true
    }
    fn assess(&self, _args: &JsonMap) -> crate::Assessment {
        let at = match (&self.path, &self.def.body) {
            (Some(p), Body::Script { language, .. }) => format!(" ({})", p.join(language.file()).display()),
            _ => String::new(),
        };
        let mut a = crate::Assessment::new(
            RiskLevel::Dangerous,
            format!("run my self-made script `{}`{at}: {}", self.def.name, self.def.description),
        );
        a.force_confirm = true;
        a
    }
    async fn execute(&self, args: &JsonMap) -> ToolResult {
        let Body::Script { language, script } = &self.def.body else {
            return ToolResult::Error("not a script tool".into());
        };
        // The script text runs as held in memory -- what was screened is what
        // runs. Editing the file on disk takes effect (re-screened) at restart.
        let mut cmd = tokio::process::Command::new(language.interpreter());
        match language {
            Language::Bash => cmd.arg("-c").arg(script).arg(&self.def.name),
            Language::Python => cmd.arg("-c").arg(script),
        };
        cmd.current_dir(arc_config::paths::home_dir())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let all = serde_json::to_string(args).unwrap_or_else(|_| "{}".into());
        cmd.env("ARC_ARGS", &all);
        for (k, v) in args {
            let text = match v {
                Json::String(s) => s.clone(),
                other => other.to_string(),
            };
            cmd.env(format!("ARC_ARG_{}", k.to_ascii_uppercase()), text);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolResult::Error(format!("could not start {}: {e}", language.interpreter())),
        };
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(all.as_bytes()).await;
        }
        let timeout = std::time::Duration::from_secs(self.timeout_s);
        let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return ToolResult::Error(e.to_string()),
            Err(_) => return ToolResult::Error(format!("timed out after {}s", timeout.as_secs())),
        };
        let clip = |b: &[u8]| {
            let s = String::from_utf8_lossy(b).trim().to_string();
            if s.chars().count() > MAX_OUTPUT_CHARS {
                format!("{}…", s.chars().take(MAX_OUTPUT_CHARS).collect::<String>())
            } else {
                s
            }
        };
        let stdout = clip(&out.stdout);
        if out.status.success() {
            ToolResult::Ok(serde_json::json!({ "output": stdout, "exit_code": 0 }))
        } else {
            let code = out.status.code().unwrap_or(-1);
            let err = clip(&out.stderr);
            ToolResult::Error(format!("exit {code}: {}", if err.is_empty() { stdout } else { err }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_config::Config;

    fn store() -> CustomTools {
        let cfg = Config::default();
        let s = CustomTools::new(ShellAnalyzer::new(&cfg.permissions.shell, &[]).unwrap(), 10);
        s.set_builtins(["media_pause", "audio_volume_mute", "workspace_goto", "reboot"].map(String::from));
        s
    }

    fn composite(name: &str, steps: Json, params: &[(&str, &str)]) -> Def {
        Def {
            name: name.into(),
            description: "test tool".into(),
            params: params.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            created: String::new(),
            fingerprint: String::new(),
            body: Body::Composite { steps: serde_json::from_value(steps).unwrap() },
        }
    }

    fn script(name: &str, language: Language, script: &str, params: &[&str]) -> Def {
        Def {
            name: name.into(),
            description: "test script".into(),
            params: params.iter().map(|p| (p.to_string(), "a value".to_string())).collect(),
            created: String::new(),
            fingerprint: String::new(),
            body: Body::Script { language, script: script.into() },
        }
    }

    #[test]
    fn names_that_could_escape_or_shadow_are_refused() {
        let s = store();
        for bad in ["../evil", "a/b", "Focus", "x", "media_pause", "tool_create", "has space", "9lives"] {
            let e = s.create(composite(bad, serde_json::json!([{"tool": "media_pause"}]), &[])).unwrap_err();
            assert!(e.contains("not a valid") || e.contains("built-in"), "{bad}: {e}");
        }
        assert!(s.list().is_empty());
    }

    #[test]
    fn composites_only_chain_real_tools_and_declared_params() {
        let s = store();
        let bad = [
            (serde_json::json!([{"tool": "no_such"}]), "unknown tool"),
            (serde_json::json!([{"tool": "tool_create"}]), "cannot create"),
            (serde_json::json!([{"tool": "loop_tool"}]), "cannot call itself"),
            (serde_json::json!([{"tool": "workspace_goto", "args": {"id": "{ws}"}}]), "no parameter `ws`"),
            (serde_json::json!([]), "1-12 steps"),
        ];
        for (steps, want) in bad {
            let e = s.create(composite("loop_tool", steps, &[])).unwrap_err();
            assert!(e.contains(want), "{want}: {e}");
        }
        s.create(composite(
            "focus",
            serde_json::json!([{"tool": "workspace_goto", "args": {"id": "{ws}"}}]),
            &[("ws", "workspace")],
        ))
        .unwrap();
        // A composite of a composite is refused; list the steps instead.
        let e = s.create(composite("focus_two", serde_json::json!([{"tool": "focus"}]), &[])).unwrap_err();
        assert!(e.contains("itself a composite"), "{e}");
    }

    #[test]
    fn fill_passes_whole_values_and_interpolates_inside_strings() {
        let args: JsonMap =
            [("ws".to_string(), serde_json::json!(3)), ("q".to_string(), serde_json::json!("rust"))].into();
        assert_eq!(fill(&serde_json::json!({"id": "{ws}"}), &args), serde_json::json!({"id": 3}));
        assert_eq!(
            fill(&serde_json::json!({"url": "https://x/?q={q}&n={ws}"}), &args),
            serde_json::json!({"url": "https://x/?q=rust&n=3"})
        );
    }

    #[test]
    fn a_bash_line_the_shell_policy_blocks_refuses_the_script() {
        let s = store();
        let e = s
            .create(script("wipe", Language::Bash, "echo start\n# harmless comment\nrm -rf /\n", &[]))
            .unwrap_err();
        assert!(e.contains("line 3"), "{e}");
        assert!(s.get("wipe").is_none());
    }

    /// A script made before fingerprints existed is recorded as-is, not
    /// treated as edited on every start.
    #[test]
    fn a_script_from_before_fingerprints_is_adopted_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let s = CustomTools::new(ShellAnalyzer::new(&Config::default().permissions.shell, &[]).unwrap(), 10);
        s.attach_dir(dir.path().to_path_buf());
        s.create(script("old", Language::Bash, "echo hi", &[])).unwrap();
        let j = dir.path().join("old/tool.json");
        let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&j).unwrap()).unwrap();
        v.as_object_mut().unwrap().remove("fingerprint");
        std::fs::write(&j, v.to_string()).unwrap();
        for _ in 0..2 {
            let s =
                CustomTools::new(ShellAnalyzer::new(&Config::default().permissions.shell, &[]).unwrap(), 10);
            s.attach_dir(dir.path().to_path_buf());
            assert!(s.changed_since_created().is_empty());
        }
        assert!(std::fs::read_to_string(&j).unwrap().contains("fingerprint"));
        std::fs::write(dir.path().join("old/run.sh"), "echo changed").unwrap();
        let s = CustomTools::new(ShellAnalyzer::new(&Config::default().permissions.shell, &[]).unwrap(), 10);
        s.attach_dir(dir.path().to_path_buf());
        assert_eq!(s.changed_since_created(), vec!["old".to_string()]);
    }

    #[test]
    fn a_shebang_alone_is_not_a_script() {
        let s = store();
        for stub in ["#!/usr/bin/env bash\n", "#!/usr/bin/env bash\n# take a screenshot\n\n", "   "] {
            let e = s.create(script("screenshot", Language::Bash, stub, &[])).unwrap_err();
            assert!(e.contains("no commands"), "{stub:?}: {e}");
        }
        assert!(s.get("screenshot").is_none());
    }

    #[test]
    fn script_tools_always_ask_and_are_dangerous_by_nature() {
        let s = store();
        s.create(script("greet", Language::Bash, "echo hi", &[])).unwrap();
        let t = s.tool("greet", &|_| RiskLevel::Safe).unwrap();
        assert_eq!(t.base_risk(), RiskLevel::Dangerous);
        let a = t.assess(&JsonMap::new());
        assert!(a.force_confirm && a.risk == RiskLevel::Dangerous && a.blocked.is_none());
    }

    #[tokio::test]
    async fn script_arguments_are_data_not_code() {
        let s = store();
        s.create(script("echo_back", Language::Bash, "printf '%s' \"$ARC_ARG_TEXT\"", &["text"])).unwrap();
        let evil = "$(touch /tmp/arc-should-not-exist); `id`";
        let args: JsonMap = [("text".to_string(), serde_json::json!(evil))].into();
        let ToolResult::Ok(v) = s.tool("echo_back", &|_| RiskLevel::Safe).unwrap().execute(&args).await
        else {
            panic!()
        };
        assert_eq!(v["output"], evil);
        assert!(!Path::new("/tmp/arc-should-not-exist").exists());
    }

    #[tokio::test]
    async fn python_scripts_get_args_as_env_and_stdin_json() {
        let s = store();
        let py = "import json, os, sys\nd = json.load(sys.stdin)\nprint(os.environ['ARC_ARG_WHO'], d['who'])";
        s.create(script("py_greet", Language::Python, py, &["who"])).unwrap();
        let args: JsonMap = [("who".to_string(), serde_json::json!("bob"))].into();
        let ToolResult::Ok(v) = s.tool("py_greet", &|_| RiskLevel::Safe).unwrap().execute(&args).await else {
            panic!()
        };
        assert_eq!(v["output"], "bob bob");
        // Script parameters are optional: an omitted one is simply unset, so
        // the script applies its own default instead of the model making one
        // up.
        let t = s.tool("py_greet", &|_| RiskLevel::Safe).unwrap();
        assert_eq!(t.parameters()["required"], serde_json::json!([]));
        s.create(script("maybe", Language::Bash, "echo \"${ARC_ARG_OUT:-default}\"", &["out"])).unwrap();
        let ToolResult::Ok(v) = s.tool("maybe", &|_| RiskLevel::Safe).unwrap().execute(&JsonMap::new()).await
        else {
            panic!()
        };
        assert_eq!(v["output"], "default");
    }

    #[tokio::test]
    async fn a_failing_script_reports_its_exit_and_stderr() {
        let s = store();
        s.create(script("fails", Language::Bash, "echo oops >&2; exit 3", &[])).unwrap();
        let ToolResult::Error(e) =
            s.tool("fails", &|_| RiskLevel::Safe).unwrap().execute(&JsonMap::new()).await
        else {
            panic!()
        };
        assert_eq!(e, "exit 3: oops");
    }

    #[test]
    fn tools_persist_reload_and_delete_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let s = store();
        assert!(s.attach_dir(dir.path().to_path_buf()).is_empty());
        s.create(script("greet", Language::Bash, "echo hi", &[])).unwrap();
        s.create(composite("quiet", serde_json::json!([{"tool": "media_pause"}, {"tool": "greet"}]), &[]))
            .unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("greet/run.sh")).unwrap(), "echo hi");
        // Overwriting is refused: that would lose the old tool without asking.
        assert!(
            s.create(script("greet", Language::Bash, "echo bye", &[]))
                .unwrap_err()
                .contains("already exists")
        );

        let again = store();
        assert!(again.attach_dir(dir.path().to_path_buf()).is_empty());
        assert_eq!(again.names(), vec!["greet", "quiet"]);
        assert_eq!(
            again.get("greet").unwrap().body,
            Body::Script { language: Language::Bash, script: "echo hi".into() }
        );

        // A tool other tools depend on cannot be pulled out from under them.
        assert!(again.delete("greet").unwrap_err().contains("used by quiet"));
        again.delete("quiet").unwrap();
        again.delete("greet").unwrap();
        assert!(!dir.path().join("greet").exists());
        assert!(again.list().is_empty());
    }

    #[test]
    fn a_tampered_script_on_disk_is_screened_again_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let s = store();
        s.attach_dir(dir.path().to_path_buf());
        s.create(script("greet", Language::Bash, "echo hi", &[])).unwrap();
        std::fs::write(dir.path().join("greet/run.sh"), "echo hi\nrm -rf /\n").unwrap();
        let again = store();
        let problems = again.attach_dir(dir.path().to_path_buf());
        assert!(problems.len() == 1 && problems[0].contains("line 2"), "{problems:?}");
        assert!(again.get("greet").is_none());
    }
}
