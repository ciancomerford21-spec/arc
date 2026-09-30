//! XDG-aware filesystem locations. Nothing here is machine-specific: every
//! path is derived from the environment at runtime.

use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() && Path::new(&v).is_absolute() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

/// `~/.config/arc` (override: `$ARC_CONFIG_DIR`).
pub fn config_dir() -> PathBuf {
    std::env::var_os("ARC_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("arc"))
}

/// `~/.local/share/arc` (override: `$ARC_DATA_DIR`). Models, venv, databases.
pub fn data_dir() -> PathBuf {
    std::env::var_os("ARC_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg("XDG_DATA_HOME", ".local/share").join("arc"))
}

/// `~/.local/state/arc` (override: `$ARC_STATE_DIR`). Logs and audit trail.
pub fn state_dir() -> PathBuf {
    std::env::var_os("ARC_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg("XDG_STATE_HOME", ".local/state").join("arc"))
}

pub fn log_dir() -> PathBuf {
    state_dir().join("logs")
}

pub fn audit_log_path() -> PathBuf {
    state_dir().join("audit.jsonl")
}

pub fn config_file() -> PathBuf {
    std::env::var_os("ARC_CONFIG").map(PathBuf::from).unwrap_or_else(|| config_dir().join("config.toml"))
}

pub fn automations_file() -> PathBuf {
    config_dir().join("automations.toml")
}

/// Per-tool safety classifications set from the Arc app
/// (override: `$ARC_TOOL_CLASSES`).
///
/// Its own file rather than a `[tools]` table in config.toml: these are
/// rewritten on every click in the UI, and a config file being rewritten
/// while the user is editing it in a text editor is a lost-comment
/// argument waiting to happen. A separate JSON file is only ever written by
/// Arc.
pub fn tool_classes_file() -> PathBuf {
    std::env::var_os("ARC_TOOL_CLASSES").map(PathBuf::from).unwrap_or_else(|| {
        config_dir().join("tool_classes.json")
    })
}

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

pub fn runtime_dir() -> PathBuf {
    arc_proto::runtime_dir()
}

/// Compact status JSON for bar widgets (watched with inotify, never polled).
pub fn bar_status_file() -> PathBuf {
    runtime_dir().join("bar.json")
}

/// Expand a leading `~` / `$HOME`. Other variables are intentionally not expanded.
pub fn expand(p: &str) -> PathBuf {
    let p = p.trim();
    if p == "~" {
        return home();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return home().join(rest);
    }
    if let Some(rest) = p.strip_prefix("$HOME/") {
        return home().join(rest);
    }
    PathBuf::from(p)
}

/// Replace the home prefix with `~` for display.
pub fn display(p: &Path) -> String {
    let h = home();
    match p.strip_prefix(&h) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

pub fn home_dir() -> PathBuf {
    home()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde() {
        let h = home();
        assert_eq!(expand("~"), h);
        assert_eq!(expand("~/Projects"), h.join("Projects"));
        assert_eq!(expand("$HOME/x"), h.join("x"));
        assert_eq!(expand("/etc"), PathBuf::from("/etc"));
        assert_eq!(display(&h.join("a/b")), "~/a/b");
    }
}
