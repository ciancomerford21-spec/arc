//! Launch apps by the name a person would say ("files", "VS Code", "foot").
//!
//! Resolution order:
//!   1. Omarchy's defaults for generic words ("terminal", "browser", "files", "editor").
//!   2. Installed desktop entries (`*.desktop` in XDG data dirs): exact id,
//!      `Name=`, `GenericName=`, `Keywords=`, executable, then fuzzy match.
//!   3. An executable of that name on PATH.
//!
//! If a window of the app already exists it is focused instead of starting a
//! second copy. Launching goes through `uwsm-app` (as Omarchy does) so the app
//! gets its own systemd scope and survives Arc restarting.

use crate::{Result, SHORT, SysError, run, which};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopApp {
    /// Desktop file id without ".desktop", e.g. "org.gnome.Nautilus".
    pub id: String,
    pub name: String,
    pub generic_name: String,
    pub keywords: Vec<String>,
    /// First word of Exec=, e.g. "nautilus".
    pub exe: String,
    pub wm_class: String,
}

impl DesktopApp {
    /// Pattern used to find an existing window of this app.
    fn window_pattern(&self) -> String {
        if !self.wm_class.is_empty() {
            return self.wm_class.clone();
        }
        // Desktop ids like "org.gnome.Nautilus" are usually the Wayland app id.
        self.id.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Desktop(DesktopApp),
    /// Plain executable on PATH (no desktop entry).
    Exe(String),
    /// An Omarchy launcher command (`omarchy-launch-browser` …).
    Command { label: String, argv: Vec<String>, window: String },
}

impl Resolved {
    pub fn label(&self) -> String {
        match self {
            Resolved::Desktop(d) => d.name.clone(),
            Resolved::Exe(e) => e.clone(),
            Resolved::Command { label, .. } => label.clone(),
        }
    }
}

fn norm(s: &str) -> String {
    s.to_lowercase()
        .replace("'s ", " ")
        .replace("’s ", " ")
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn compact(s: &str) -> String {
    norm(s).replace(' ', "")
}

/// Words people add that aren't part of the app name.
fn strip_filler(q: &str) -> String {
    let drop = ["the", "app", "application", "program", "my", "a", "an", "please", "up"];
    let mut words: Vec<&str> = q.split_whitespace().filter(|w| !drop.contains(w)).collect();
    // "foot terminal" -> "foot", "firefox browser" -> "firefox": a trailing type
    // word after a specific name. Kept when it's the whole query ("terminal").
    let kinds = ["terminal", "browser", "editor"];
    if words.len() >= 2 && kinds.contains(words.last().unwrap()) {
        words.pop();
    }
    words.join(" ")
}

/// Spoken aliases -> something that matches a desktop entry or command.
fn alias(q: &str) -> Option<&'static str> {
    Some(match q {
        "vs code" | "vscode" | "visual studio code" | "visual studio" | "code editor" => "code",
        "file manager" | "file browser" | "files" | "my files" => "@files",
        "terminal" | "a terminal" | "console" | "shell" => "@terminal",
        "browser" | "web browser" | "internet" => "@browser",
        "editor" | "text editor" => "@editor",
        "hermes" | "hermes desktop" => "hermes-desktop",
        _ => return None,
    })
}

fn omarchy_default(kind: &str) -> Option<Resolved> {
    let (label, cmd, window) = match kind {
        "@terminal" => ("terminal", "omarchy-launch-terminal", ""),
        "@browser" => ("browser", "omarchy-launch-browser", ""),
        "@editor" => ("editor", "omarchy-launch-editor", ""),
        "@files" => ("Files", "nautilus", "org.gnome.Nautilus"),
        _ => return None,
    };
    if which(cmd).is_none() {
        return None;
    }
    Some(Resolved::Command { label: label.into(), argv: vec![cmd.into()], window: window.into() })
}

// ---------------------------------------------------------------------------
// Desktop entries
// ---------------------------------------------------------------------------

fn data_dirs() -> Vec<PathBuf> {
    let mut v = vec![];
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    v.push(std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share")));
    let sys = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    v.extend(sys.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    v.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    v.push(home.join(".local/share/flatpak/exports/share"));
    v
}

/// Parse the `[Desktop Entry]` group. Returns None for hidden/NoDisplay/non-apps.
pub fn parse_desktop(id: &str, text: &str) -> Option<DesktopApp> {
    let mut in_entry = false;
    let mut kv: HashMap<&str, &str> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            // Keep the unlocalised key only (Name, not Name[de]).
            kv.entry(k.trim()).or_insert(v.trim());
        }
    }
    if kv.get("Type").copied().unwrap_or("Application") != "Application" {
        return None;
    }
    if kv.get("NoDisplay") == Some(&"true") || kv.get("Hidden") == Some(&"true") {
        return None;
    }
    let exec = kv.get("Exec")?;
    let exe = shlex::split(exec)
        .unwrap_or_default()
        .into_iter()
        .find(|w| !w.contains('=') || w.starts_with('/'))
        .map(|w| Path::new(&w).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or(w))
        .unwrap_or_default();
    Some(DesktopApp {
        id: id.to_string(),
        name: kv.get("Name").unwrap_or(&id).to_string(),
        generic_name: kv.get("GenericName").unwrap_or(&"").to_string(),
        keywords: kv.get("Keywords").map(|k| k.split(';').filter(|s| !s.is_empty()).map(str::to_string).collect()).unwrap_or_default(),
        exe,
        wm_class: kv.get("StartupWMClass").unwrap_or(&"").to_string(),
    })
}

pub fn desktop_apps() -> Vec<DesktopApp> {
    let mut seen: HashMap<String, DesktopApp> = HashMap::new();
    for dir in data_dirs() {
        let Ok(rd) = std::fs::read_dir(dir.join("applications")) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("desktop") {
                continue;
            }
            let id = p.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            // Earlier dirs (user) win over later ones (system).
            if seen.contains_key(&id) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p) {
                if let Some(app) = parse_desktop(&id, &text) {
                    seen.insert(id, app);
                }
            }
        }
    }
    seen.into_values().collect()
}

/// Score how well `app` matches the spoken query (higher is better; 0 = no match).
fn score(app: &DesktopApp, q: &str) -> u32 {
    let qc = compact(q);
    if qc.is_empty() {
        return 0;
    }
    let name = compact(&app.name);
    let id = compact(&app.id);
    let id_tail = compact(app.id.rsplit('.').next().unwrap_or(&app.id));
    let exe = compact(&app.exe);
    if qc == exe || qc == id || qc == id_tail {
        return 100;
    }
    if qc == name {
        return 95;
    }
    if !app.generic_name.is_empty() && qc == compact(&app.generic_name) {
        return 80;
    }
    if app.keywords.iter().any(|k| compact(k) == qc) {
        return 70;
    }
    if name.starts_with(&qc) || (qc.len() >= 4 && name.contains(&qc)) {
        return 60;
    }
    if qc.len() >= 4 && (exe.starts_with(&qc) || id.contains(&qc)) {
        return 50;
    }
    let sim = strsim::jaro_winkler(&qc, &name).max(strsim::jaro_winkler(&qc, &exe));
    if sim >= 0.9 { (sim * 45.0) as u32 } else { 0 }
}

/// Resolve a spoken app name against `apps`. Pure: used by tests.
pub fn resolve_in(query: &str, apps: &[DesktopApp], exe_on_path: impl Fn(&str) -> bool) -> Option<Resolved> {
    let n = norm(query);
    let q = strip_filler(&n);
    if q.is_empty() {
        return None;
    }
    // Aliases see the phrase before the trailing type word is dropped ("web browser").
    let target = alias(&n)
        .or_else(|| alias(&q))
        .map(str::to_string)
        .unwrap_or_else(|| q.clone());
    if target.starts_with('@') {
        if let Some(r) = omarchy_default(&target) {
            return Some(r);
        }
    }
    let target = target.trim_start_matches('@');
    let best = apps.iter().map(|a| (score(a, target), a)).filter(|(s, _)| *s > 0).max_by(|a, b| {
        // Prefer higher score, then shorter names ("Files" over "Files (autorun)").
        a.0.cmp(&b.0).then_with(|| b.1.name.len().cmp(&a.1.name.len()))
    });
    if let Some((_, a)) = best {
        return Some(Resolved::Desktop(a.clone()));
    }
    let exe = target.replace(' ', "-");
    exe_on_path(&exe).then_some(Resolved::Exe(exe))
}

pub fn resolve(query: &str) -> Option<Resolved> {
    resolve_in(query, &desktop_apps(), |e| which(e).is_some())
}

/// Close matches, for "did you mean" error messages.
pub fn suggestions(query: &str, n: usize) -> Vec<String> {
    let q = compact(&strip_filler(&norm(query)));
    let mut v: Vec<(f64, String)> = desktop_apps()
        .into_iter()
        .map(|a| (strsim::jaro_winkler(&q, &compact(&a.name)), a.name))
        .collect();
    v.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    v.into_iter().take(n).map(|(_, s)| s).collect()
}

// ---------------------------------------------------------------------------
// Launching
// ---------------------------------------------------------------------------

async fn existing_window(pattern: &str) -> Option<String> {
    if pattern.is_empty() {
        return None;
    }
    let out = run("hyprctl", &["clients", "-j"], SHORT).await.ok()?;
    let clients: Vec<serde_json::Value> = serde_json::from_str(&out).ok()?;
    let p = pattern.to_lowercase();
    clients.iter().find_map(|c| {
        let class = c.get("class")?.as_str()?.to_lowercase();
        let init = c.get("initialClass").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
        (class == p || init == p).then(|| c.get("address")?.as_str().map(str::to_string))?
    })
}

async fn focus(address: &str) -> Result<()> {
    let lua = format!("hl.dsp.focus({{ window = \"address:{address}\" }})");
    run("hyprctl", &["dispatch", &lua], SHORT).await.map(|_| ())
}

fn spawn_detached(argv: &[String]) -> Result<()> {
    let mut full: Vec<String> = vec![];
    if which("uwsm-app").is_some() {
        full.extend(["uwsm-app".into(), "--".into()]);
    }
    full.extend(argv.iter().cloned());
    let mut cmd = std::process::Command::new("setsid");
    cmd.arg("-f").args(&full);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let status = cmd.status().map_err(|e| SysError::Command { what: full.join(" "), detail: e.to_string() })?;
    if !status.success() {
        return Err(SysError::Command { what: full.join(" "), detail: format!("exit status {status}") });
    }
    Ok(())
}

/// What happened, phrased for speech.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launched {
    Started(String),
    Focused(String),
}

impl Launched {
    pub fn sentence(&self) -> String {
        match self {
            Launched::Started(n) => format!("Opening {n}"),
            Launched::Focused(n) => format!("{n} is already open; switched to it"),
        }
    }
}

pub async fn launch_resolved(r: &Resolved) -> Result<Launched> {
    let (window, argv): (String, Vec<String>) = match r {
        Resolved::Desktop(d) => {
            let argv = if which("gtk-launch").is_some() {
                vec!["gtk-launch".into(), d.id.clone()]
            } else {
                vec![d.exe.clone()]
            };
            (d.window_pattern(), argv)
        }
        Resolved::Exe(e) => (e.clone(), vec![e.clone()]),
        Resolved::Command { argv, window, .. } => (window.clone(), argv.clone()),
    };
    if let Some(addr) = existing_window(&window).await {
        focus(&addr).await?;
        return Ok(Launched::Focused(r.label()));
    }
    spawn_detached(&argv)?;
    Ok(Launched::Started(r.label()))
}

/// Resolve and launch. Errors list close matches so the caller can retry once.
pub async fn launch(query: &str) -> Result<Launched> {
    match resolve(query) {
        Some(r) => launch_resolved(&r).await,
        None => {
            let s = suggestions(query, 3);
            Err(SysError::Command {
                what: format!("launch {query}"),
                detail: format!(
                    "no installed app matches \"{query}\". Closest installed apps: {}. \
                     Do not retry with other guesses; ask the user which one they meant.",
                    s.join(", ")
                ),
            })
        }
    }
}

/// Normalise what the model passes ("github.com", "https://x.org/a") into an
/// http(s) URL. Anything else (file:, javascript:, shell text) is refused.
pub fn normalise_url(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let lower = s.to_lowercase();
    let url = if lower.starts_with("https://") || lower.starts_with("http://") {
        s.to_string()
    } else if lower.contains("://") || lower.starts_with("javascript:") || lower.starts_with("file:") || lower.starts_with("data:") {
        return None;
    } else {
        format!("https://{s}")
    };
    // Host must look like a domain (or localhost).
    let host = url.split("://").nth(1)?.split(['/', '?', '#']).next()?.split('@').last()?;
    let host = host.split(':').next()?;
    let ok = host == "localhost" || (host.contains('.') && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'));
    ok.then_some(url)
}

/// Open a web page in the default browser (via Omarchy's launcher when present).
pub fn open_url(input: &str) -> Result<String> {
    let url = normalise_url(input).ok_or_else(|| SysError::Command {
        what: "open_url".into(),
        detail: format!("\"{input}\" is not a web address; pass something like github.com or https://…"),
    })?;
    let argv: Vec<String> = if which("omarchy-launch-browser").is_some() {
        vec!["omarchy-launch-browser".into(), url.clone()]
    } else {
        vec!["xdg-open".into(), url.clone()]
    };
    spawn_detached(&argv)?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str, exe: &str) -> DesktopApp {
        DesktopApp { id: id.into(), name: name.into(), generic_name: String::new(), keywords: vec![], exe: exe.into(), wm_class: String::new() }
    }

    fn apps() -> Vec<DesktopApp> {
        vec![
            app("code", "Visual Studio Code", "code"),
            app("org.gnome.Nautilus", "Files", "nautilus"),
            app("foot", "Foot", "foot"),
            app("footclient", "Foot Client", "footclient"),
            app("hermes-desktop", "Hermes", "hermes-desktop"),
            DesktopApp { generic_name: "Web Browser".into(), ..app("chromium", "Chromium", "chromium") },
            DesktopApp { keywords: vec!["spreadsheet".into()], ..app("libreoffice-calc", "LibreOffice Calc", "libreoffice") },
        ]
    }

    fn name_of(q: &str) -> Option<String> {
        resolve_in(q, &apps(), |_| false).map(|r| match r {
            Resolved::Desktop(d) => d.id,
            other => other.label(),
        })
    }

    #[test]
    fn resolves_spoken_names() {
        assert_eq!(name_of("Visual Studio Code").as_deref(), Some("code"));
        assert_eq!(name_of("vs code").as_deref(), Some("code"));
        assert_eq!(name_of("nautilus").as_deref(), Some("org.gnome.Nautilus"));
        assert_eq!(name_of("foot's terminal").as_deref(), Some("foot"));
        assert_eq!(name_of("Foot").as_deref(), Some("foot"));
        assert_eq!(name_of("Hermes Desktop").as_deref(), Some("hermes-desktop"));
        assert_eq!(name_of("hermes").as_deref(), Some("hermes-desktop"));
        // Generic "web browser" goes to Omarchy's default launcher when present, else the entry.
        assert!(matches!(name_of("web browser").as_deref(), Some("chromium") | Some("browser")));
        assert_eq!(name_of("spreadsheet").as_deref(), Some("libreoffice-calc"));
        assert_eq!(name_of("the chromium app").as_deref(), Some("chromium"));
    }

    #[test]
    fn unknown_app_is_none() {
        assert_eq!(name_of("thunar"), None);
        assert_eq!(name_of(""), None);
    }

    #[test]
    fn exe_on_path_is_last_resort() {
        let r = resolve_in("btop", &apps(), |e| e == "btop");
        assert_eq!(r, Some(Resolved::Exe("btop".into())));
    }

    #[test]
    fn parses_desktop_file() {
        let t = "[Desktop Entry]\nType=Application\nName=Files\nName[de]=Dateien\nExec=nautilus --new-window %U\nKeywords=folder;manager;\nStartupWMClass=org.gnome.Nautilus\n[Desktop Action new]\nName=New\nExec=other\n";
        let a = parse_desktop("org.gnome.Nautilus", t).unwrap();
        assert_eq!(a.name, "Files");
        assert_eq!(a.exe, "nautilus");
        assert_eq!(a.keywords, vec!["folder", "manager"]);
        assert_eq!(a.wm_class, "org.gnome.Nautilus");
        assert!(parse_desktop("x", "[Desktop Entry]\nType=Application\nName=X\nExec=x\nNoDisplay=true\n").is_none());
        let env = parse_desktop("y", "[Desktop Entry]\nName=Y\nExec=env FOO=1 /usr/bin/yapp %F\n").unwrap();
        assert_eq!(env.exe, "env");
    }

    #[test]
    fn real_desktop_entries_resolve_when_installed() {
        // Only meaningful on a machine with these apps; skips otherwise.
        let all = desktop_apps();
        if all.iter().any(|a| a.id == "code") {
            assert!(matches!(resolve("visual studio code"), Some(Resolved::Desktop(d)) if d.id == "code"));
        }
    }

    #[test]
    fn urls_are_normalised_and_unsafe_ones_refused() {
        assert_eq!(normalise_url("github.com").as_deref(), Some("https://github.com"));
        assert_eq!(normalise_url("https://www.google.com/search?q=arc").as_deref(), Some("https://www.google.com/search?q=arc"));
        assert_eq!(normalise_url("http://localhost:8080/x").as_deref(), Some("http://localhost:8080/x"));
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "rm -rf ~", "", "github", "ftp://x.org", "a.com; ls"] {
            assert_eq!(normalise_url(bad), None, "{bad}");
        }
    }
}
