//! Low-level storage helpers: path split, persistence, helpers.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{MemoryEntry, SessionSummary};

pub(super) fn split_path(path: &Path) -> (PathBuf, PathBuf) {
    let base = path.parent().unwrap_or_else(|| Path::new(".")).join("memory");
    let facts = base.join("facts.json");
    let sessions = base.join("sessions.json");
    (facts, sessions)
}

pub(super) fn persist_facts(path: &Path, entries: &[MemoryEntry]) {
    if let Some(parent) = path.parent() { let _ = fs::create_dir_all(parent); }
    let _ = fs::write(path, serde_json::to_string_pretty(entries).unwrap_or_default());
}

pub(super) fn persist_sessions(path: &Path, sessions: &[SessionSummary]) {
    if let Some(parent) = path.parent() { let _ = fs::create_dir_all(parent); }
    let _ = fs::write(path, serde_json::to_string_pretty(sessions).unwrap_or_default());
}
