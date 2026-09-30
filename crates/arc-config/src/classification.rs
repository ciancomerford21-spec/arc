//! Per-tool safety classifications, edited from the Arc app's tool list.
//!
//! Every tool ships with a built-in risk level (`Tool::base_risk`). The user
//! can override any of them from the UI without touching a config file:
//! `reboot` can be marked `safe` if they trust their own assistant, `window_list`
//! can be marked `dangerous` if they want to be asked before it reads anything.
//!
//! Stored as JSON in [`paths::tool_classes_file`] so the setting survives a
//! restart, and so the daemon (the only writer) never has to rewrite the
//! user's hand-edited `config.toml`.
//!
//! ```json
//! { "reboot": "safe", "window_list": "dangerous" }
//! ```
//!
//! An entry is a *classification*, not a permission: the runtime still refuses
//! anything a tool blocks outright, and a `dangerous` classification is
//! enforced on top of whatever policy says (see `arc_core::gate`).

use crate::paths;
use arc_proto::RiskLevel;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Tool name → the user's classification. Tools absent from the map keep
/// their built-in level.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolClasses {
    pub map: BTreeMap<String, RiskLevel>,
}

impl ToolClasses {
    pub fn get(&self, tool: &str) -> Option<RiskLevel> {
        self.map.get(tool).copied()
    }

    /// The user's classification, or `base` when they have not set one.
    pub fn effective(&self, tool: &str, base: RiskLevel) -> RiskLevel {
        self.get(tool).unwrap_or(base)
    }

    /// Set (or with `None`, clear) a tool's classification. Returns true when
    /// the map changed. Does not write to disk — see [`save`].
    pub fn set(&mut self, tool: &str, level: Option<RiskLevel>) -> bool {
        let before = self.map.get(tool).copied();
        match level {
            Some(l) => {
                if before == Some(l) {
                    return false;
                }
                self.map.insert(tool.to_string(), l);
            }
            // Clearing an absent entry is a no-op, not a rewrite of the file.
            None => match self.map.remove(tool) {
                Some(_) => return true,
                None => return false,
            },
        }
        true
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Load from disk. A missing file is an empty map, not an error: nobody
    /// has set a classification yet. A corrupt one is reported (the caller
    /// logs it) and also treated as empty, because refusing to start the
    /// assistant over a bad preference file is the wrong trade.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(c) => (c, None),
                Err(e) => (
                    Self::default(),
                    Some(format!("{}: {e} (ignored)", path.display())),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(e) => (Self::default(), Some(format!("{}: {e} (ignored)", path.display()))),
        }
    }

    /// Persist atomically. An empty map removes the file rather than leaving
    /// `{}` behind, so "no overrides" looks like no file at all.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if self.map.is_empty() {
            match std::fs::remove_file(path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let mut text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        text.push('\n');
        crate::atomic_write(path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// A loaded store plus the file it came from, so a change can be written
/// back without every caller having to remember the path.
#[derive(Debug, Clone)]
pub struct ClassifiedTools {
    path: PathBuf,
    classes: ToolClasses,
}

impl ClassifiedTools {
    /// Load from the default location, or from an explicit path.
    pub fn load_default() -> Self {
        Self::load_from(paths::tool_classes_file())
    }

    pub fn load_from(path: PathBuf) -> Self {
        let (classes, warning) = ToolClasses::load(&path);
        if let Some(w) = warning {
            // arc-config installs no subscriber of its own; the daemon has
            // already done it by the time anything is loaded.
            tracing::warn!(error = %w, "tool classifications");
        }
        Self { path, classes }
    }

    pub fn in_memory() -> Self {
        Self { path: PathBuf::new(), classes: ToolClasses::default() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, tool: &str) -> Option<RiskLevel> {
        self.classes.get(tool)
    }

    pub fn effective(&self, tool: &str, base: RiskLevel) -> RiskLevel {
        self.classes.effective(tool, base)
    }

    pub fn map(&self) -> &BTreeMap<String, RiskLevel> {
        &self.classes.map
    }

    /// Set or clear a classification and persist it. An in-memory store
    /// (no path) changes without writing, which is what tests and the
    /// `--no-persist` path use.
    pub fn set(&mut self, tool: &str, level: Option<RiskLevel>) -> Result<bool, String> {
        if !self.classes.set(tool, level) {
            return Ok(false);
        }
        if !self.path.as_os_str().is_empty() {
            self.classes.save(&self.path)?;
        }
        Ok(true)
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (c, w) = ToolClasses::load(&dir.path().join("nope.json"));
        assert!(c.is_empty());
        assert!(w.is_none(), "a file nobody has written yet is not a problem: {w:?}");
    }

    #[test]
    fn corrupt_file_is_reported_and_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.json");
        std::fs::write(&p, "{not json").unwrap();
        let (c, w) = ToolClasses::load(&p);
        assert!(c.is_empty());
        assert!(w.is_some(), "a corrupt file must be reported, not swallowed");
    }

    #[test]
    fn classifications_survive_a_reload() {
        // The whole point of the file: the setting is still there next time.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tool_classes.json");
        let mut s = ClassifiedTools::load_from(p.clone());
        s.set("window_list", Some(RiskLevel::Dangerous)).unwrap();
        s.set("reboot", Some(RiskLevel::Safe)).unwrap();

        let back = ClassifiedTools::load_from(p);
        assert_eq!(back.get("window_list"), Some(RiskLevel::Dangerous));
        assert_eq!(back.get("reboot"), Some(RiskLevel::Safe));
        assert_eq!(back.get("lock"), None, "an unset tool has no override");
    }

    #[test]
    fn clearing_removes_the_entry_and_empties_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tool_classes.json");
        let mut s = ClassifiedTools::load_from(p.clone());
        s.set("lock", Some(RiskLevel::Caution)).unwrap();
        assert!(p.exists());
        assert!(s.set("lock", None).unwrap());
        assert_eq!(s.get("lock"), None);
        assert!(!p.exists(), "an empty classification map should leave no file");
        // Clearing again changes nothing and is not an error.
        assert!(!s.set("lock", None).unwrap());
    }

    #[test]
    fn setting_the_same_level_is_not_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = ClassifiedTools::load_from(dir.path().join("t.json"));
        assert!(s.set("lock", Some(RiskLevel::Caution)).unwrap());
        assert!(!s.set("lock", Some(RiskLevel::Caution)).unwrap(), "no-op writes are churn");
    }

    #[test]
    fn an_unset_tool_keeps_its_built_in_level() {
        let c = ToolClasses::default();
        assert_eq!(c.effective("reboot", RiskLevel::Dangerous), RiskLevel::Dangerous);
        let c = ToolClasses { map: BTreeMap::from([("reboot".into(), RiskLevel::Safe)]) };
        assert_eq!(c.effective("reboot", RiskLevel::Dangerous), RiskLevel::Safe);
        // Other tools are untouched by one tool's override.
        assert_eq!(c.effective("lock", RiskLevel::Safe), RiskLevel::Safe);
    }

    #[test]
    fn the_file_is_plain_json_a_human_can_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.json");
        let mut s = ClassifiedTools::load_from(p.clone());
        s.set("reboot", Some(RiskLevel::Safe)).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("\"reboot\""), "{text}");
        assert!(text.contains("\"safe\""), "{text}");
        let back: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back["reboot"], "safe");
    }
}
