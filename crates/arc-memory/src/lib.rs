//! Small JSON file memory store for the daemon.
//!
//! Stores remembered facts and session transcripts. Persists to
//! `~/.local/share/arc/memory/`. Two parts:
//! * facts — `MemoryEntry`, append-only, searchable, survive restarts.
//! * sessions — transcripts of user + arc turns, grouped by session, survive
//!   restarts so the LLM can be primed with recent conversation context.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

const MAX_TURNS_IN_PROMPT: usize = 24;

/// One remembered fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// Unique id (generated at store time).
    pub id: String,
    /// Plain-language fact. Sanitised before storage (no secrets).
    pub fact: String,
    /// Free-text tags the user may have attached (e.g. "work", "music").
    #[serde(default)]
    pub tags: Vec<String>,
    /// When it was remembered, ISO 8601 UTC.
    pub remembered_at: String,
    /// Rolling counter so the store can prune oldest entries first.
    pub seq: u64,
}

/// One turn in a session transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTurn {
    pub seq: u64,
    pub role: SessionTurnRole,
    pub text: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionTurnRole {
    User,
    Assistant,
    System,
}

/// A session summary: short enough for the prompt, with enough for
/// `search_session`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub turns: Vec<SessionTurn>,
    #[serde(default)]
    pub last_fact: Option<String>,
    pub total_turns: usize,
}

/// The in-memory + on-disk store.
pub struct MemoryStore {
    path: PathBuf,
    sessions_path: PathBuf,
    entries: Mutex<Vec<MemoryEntry>>,
    counter: Mutex<u64>,
    sessions: Mutex<Vec<SessionSummary>>,
    session_counter: Mutex<u64>,
}

impl MemoryStore {
    /// A store at `path`, where `path` is either a directory (facts and
    /// sessions live inside it) or the facts file itself. Ambiguous for a
    /// directory that does not exist yet — prefer [`MemoryStore::new_in_dir`]
    /// when you already know it is a directory.
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        if path.extension() == Some("json".as_ref()) {
            let sessions = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("sessions.json");
            Self::with_paths(path.to_path_buf(), sessions)
        } else {
            Self::new_in_dir(path)
        }
    }

    /// A store rooted at `dir`: `dir/facts.json` and `dir/sessions.json`.
    /// Unambiguous — unlike [`MemoryStore::new`], this never inspects the
    /// filesystem, so it also works for a directory that does not exist yet.
    pub fn new_in_dir(dir: impl AsRef<Path>) -> Self {
        let base = dir.as_ref().to_path_buf();
        Self::with_paths(base.join("facts.json"), base.join("sessions.json"))
    }

    fn with_paths(facts_path: PathBuf, sessions_path: PathBuf) -> Self {
        let entries = load(&facts_path);
        let max_seq = next_seq(&entries);
        let (sessions, session_counter) = load_sessions(&sessions_path);
        Self {
            path: facts_path,
            sessions_path,
            entries: Mutex::new(entries),
            counter: Mutex::new(max_seq),
            sessions: Mutex::new(sessions),
            session_counter: Mutex::new(session_counter),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<MemoryEntry>> {
        self.entries.lock().expect("memory facts poisoned")
    }

    fn sessions_lock(&self) -> std::sync::MutexGuard<'_, Vec<SessionSummary>> {
        self.sessions.lock().expect("memory sessions poisoned")
    }

    /// Append a fact. Returns the new entry (with id, timestamp, seq).
    pub fn remember(&self, fact: String, tags: Vec<String>) -> MemoryEntry {
        let mut entries = self.lock();
        let seq = {
            let mut c = self.counter.lock().expect("counter poisoned");
            *c += 1;
            *c
        };
        let entry = MemoryEntry {
            id: uuid(),
            fact,
            tags,
            remembered_at: now_iso(),
            seq,
        };
        entries.push(entry.clone());
        drop(entries);
        self.persist_from_locked();
        entry
    }

    /// Forget by id. Returns true if something was removed.
    pub fn forget(&self, id: &str) -> bool {
        let mut entries = self.lock();
        let n = entries.len();
        entries.retain(|e| e.id != id);
        if entries.len() < n {
            drop(entries);
            self.persist_from_locked();
            true
        } else {
            false
        }
    }

    /// Drop every fact. Returns how many were removed.
    pub fn clear(&self) -> usize {
        let mut entries = self.lock();
        let n = entries.len();
        entries.clear();
        drop(entries);
        if n > 0 {
            self.persist_from_locked();
        }
        n
    }

    /// All facts, oldest first.
    pub fn list(&self) -> Vec<MemoryEntry> {
        self.lock().clone()
    }

    /// Search facts that contain every given token (case-insensitive).
    pub fn search(&self, terms: Vec<&str>) -> Vec<MemoryEntry> {
        let lower_terms: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
        let entries = self.lock();
        entries
            .iter()
            .filter(|e| {
                if terms.is_empty() {
                    true
                } else {
                    lower_terms
                        .iter()
                        .all(|t| e.fact.to_lowercase().contains(t.as_str()))
                }
            })
            .cloned()
            .collect()
    }

    /// Summarize the most recent conversational turns into a short block the LLM
    /// can read. Oldest turns dropped first; capped at `MAX_TURNS_IN_PROMPT`.
    /// Only surfaced when `send_conversation_context` is enabled.
    pub fn recent_conversation(&self) -> String {
        let sessions = self.sessions_lock();
        if sessions.is_empty() {
            return String::new();
        }
        let mut turns = Vec::new();
        for s in sessions.iter().rev() {
            for t in s.turns.iter().rev() {
                if turns.len() >= MAX_TURNS_IN_PROMPT {
                    break;
                }
                turns.push(t);
            }
            if turns.len() >= MAX_TURNS_IN_PROMPT {
                break;
            }
        }
        if turns.is_empty() {
            return String::new();
        }
        let lines: Vec<String> = turns
            .iter()
            .rev()
            .map(|t| match t.role {
                SessionTurnRole::User => format!("user: {}", t.text),
                SessionTurnRole::Assistant => format!("arc: {}", t.text),
                SessionTurnRole::System => format!("system: {}", t.text),
            })
            .collect();
        format!(
            "RECENT CONVERSATION ({} turns across {} sessions):\n{}\n",
            turns.len(),
            sessions.len(),
            lines.join("\n")
        )
    }

    /// Write one turn into the most recent session.
    pub fn append_turn(
        &self,
        role: SessionTurnRole,
        text: String,
    ) -> SessionSummary {
        let mut sessions = self.sessions_lock();
        let seq = {
            let mut c = self.session_counter.lock().expect("session counter poisoned");
            *c += 1;
            *c
        };
        let created_at = now_iso();
        let turn = SessionTurn {
            seq,
            role,
            text,
            tags: vec![],
            created_at: created_at.clone(),
        };
        if let Some(last) = sessions.last_mut() {
            last.turns.push(turn);
            let updated_at = created_at.clone();
            last.updated_at = updated_at;
            let out = SessionSummary {
                id: last.id.clone(),
                created_at: last.created_at.clone(),
                updated_at: created_at,
                turns: last.turns.clone(),
                last_fact: last.last_fact.clone(),
                total_turns: last.turns.len(),
            };
            drop(sessions);
            self.persist_sessions_from_locked();
            out
        } else {
            let summary = SessionSummary {
                id: uuid(),
                created_at: created_at.clone(),
                updated_at: created_at,
                turns: vec![turn],
                last_fact: None,
                total_turns: 1,
            };
            sessions.push(summary.clone());
            drop(sessions);
            self.persist_sessions_from_locked();
            summary
        }
    }

    fn persist_from_locked(&self) {
        let entries = self.lock();
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(
            &self.path,
            serde_json::to_string_pretty(&*entries).unwrap_or_default(),
        );
    }

    fn persist_sessions_from_locked(&self) {
        let sessions = self.sessions_lock();
        if let Some(parent) = self.sessions_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(
            &self.sessions_path,
            serde_json::to_string_pretty(&*sessions).unwrap_or_default(),
        );
    }

    /// Search a session by id, returning the most recent N turns that match
    /// every given token (case-insensitive).
    pub fn search_session(
        &self,
        session_id: &str,
        terms: Vec<&str>,
        limit: usize,
    ) -> Option<Vec<SessionTurn>> {
        let sessions = self.sessions_lock();
        let session = sessions.iter().find(|s| s.id == session_id)?;
        let lower_terms: Vec<String> =
            terms.iter().map(|t| t.to_lowercase()).collect();
        let mut matched: Vec<SessionTurn> = session
            .turns
            .iter()
            .rev()
            .filter(|t| {
                if terms.is_empty() {
                    true
                } else {
                    lower_terms
                        .iter()
                        .all(|term| t.text.to_lowercase().contains(term.as_str()))
                }
            })
            .take(limit)
            .cloned()
            .collect();
        matched.reverse();
        Some(matched)
    }

    pub fn prompt_block(
        &self,
        max_entries: usize,
        filter: Option<Vec<&str>>,
    ) -> String {
        let candidates = match filter {
            Some(terms) => self.search(terms),
            None => self.list(),
        };
        if candidates.is_empty() {
            return String::new();
        }
        let kept = if candidates.len() > max_entries {
            &candidates[candidates.len() - max_entries..]
        } else {
            &candidates
        };
        let lines: Vec<String> = kept.iter().map(|e| format!("- {}", e.fact)).collect();
        format!(
            "MEMORIZED FACTS ({} known):\n{}\n",
            kept.len(),
            lines.join("\n")
        )
    }
}

fn uuid() -> String {
    // Short unique-ish id without pulling in uuid: 10 hex chars from random.
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(10);
    for _ in 0..10 {
        let r = fast_random();
        let h = r.iter().fold(0u64, |acc, &b| acc.wrapping_add(b as u64));
        s.push(HEX[(h & 0xf) as usize] as char);
    }
    s
}

fn fast_random() -> [u8; 8] {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut t =
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
    // cheap xorshift
    t ^= t << 7;
    t ^= t >> 9;
    t ^= t << 13;
    t.to_le_bytes()
}

fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs =
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
    unix_to_iso(secs)
}

fn unix_to_iso(secs: u64) -> String {
    let mut days = secs / 86400;
    let sec_of_day = secs % 86400;
    let (year, month, day) = days_to_ymd(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month,
        day,
        sec_of_day / 3600,
        (sec_of_day % 3600) / 60,
        sec_of_day % 60
    )
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut year = 1970u64;
    loop {
        let n = days_in_year(year);
        if days < n {
            break;
        }
        days -= n;
        year += 1;
    }
    let month_days = [
        31,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u64;
    for &md in &month_days {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }
    (year, month, days + 1)
}

fn days_in_year(y: u64) -> u64 {
    if is_leap(y) { 366 } else { 365 }
}
fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn next_seq(entries: &[MemoryEntry]) -> u64 {
    entries.iter().map(|e| e.seq).max().unwrap_or(0)
}

fn load(path: &Path) -> Vec<MemoryEntry> {
    if !path.exists() {
        return vec![];
    }
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|_| vec![]),
        Err(_) => vec![],
    }
}

fn load_sessions(path: &Path) -> (Vec<SessionSummary>, u64) {
    if !path.exists() {
        return (vec![], 0);
    }
    match fs::read_to_string(path) {
        Ok(text) => {
            let v: Vec<SessionSummary> =
                serde_json::from_str(&text).unwrap_or_else(|_| vec![]);
            let n = v
                .iter()
                .flat_map(|s| s.turns.iter().map(|t| t.seq))
                .max()
                .unwrap_or(0);
            (v, n)
        }
        Err(_) => (vec![], 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique, empty store per call. Tests run in parallel, so they must
    /// not share a directory.
    fn tmp_store(tag: &str) -> MemoryStore {
        let dir = std::env::temp_dir().join(format!("arc_memory_test_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        MemoryStore::new_in_dir(&dir)
    }

    #[test]
    fn remember_and_list() {
        let store = tmp_store("list");
        let e = store.remember("I use workspace 3 for music".into(), vec!["music".into()]);
        assert!(!e.id.is_empty());
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.list()[0].fact, "I use workspace 3 for music");
    }

    #[test]
    fn forget_removes_entry() {
        let store = tmp_store("forget");
        let e = store.remember("temp".into(), vec![]);
        assert!(store.forget(&e.id));
        assert!(store.list().is_empty());
    }

    #[test]
    fn search_matches() {
        let store = tmp_store("search");
        store.remember("I use bspwm on this machine".into(), vec![]);
        store.remember("I drink tea, not coffee".into(), vec![]);
        let r = store.search(vec!["tea"]);
        assert_eq!(r.len(), 1);
        assert!(r[0].fact.contains("tea"));
    }

    #[test]
    fn prompt_block_truncates() {
        let store = tmp_store("prompt");
        for i in 0..5 {
            store.remember(format!("fact {i}"), vec![]);
        }
        let block = store.prompt_block(2, None);
        assert!(block.contains("fact 3") && block.contains("fact 4"));
        assert!(!block.contains("fact 0"));
    }

    #[test]
    fn persistence_across_instances() {
        let dir = std::env::temp_dir().join(format!("arc_memory_persist_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        {
            let s = MemoryStore::new_in_dir(&dir);
            s.remember("persistent fact".into(), vec!["x".into()]);
        }
        let s2 = MemoryStore::new_in_dir(&dir);
        assert_eq!(s2.list().len(), 1);
        assert_eq!(s2.list()[0].fact, "persistent fact");
    }

    #[test]
    fn conversation_persists() {
        let dir = std::env::temp_dir().join(format!("arc_memory_convo_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        {
            let s = MemoryStore::new_in_dir(&dir);
            s.append_turn(SessionTurnRole::User, "open VS Code".into());
            s.append_turn(SessionTurnRole::Assistant, "Done.".into());
        }
        let s = MemoryStore::new_in_dir(&dir);
        let recent = s.recent_conversation();
        assert!(recent.contains("open VS Code") && recent.contains("Done."));
    }

    #[test]
    fn recent_conversation_capped_to_max_turns() {
        let store = tmp_store("cap");
        for i in 0..60 {
            store.append_turn(SessionTurnRole::User, format!("user turn {i}"));
            store.append_turn(SessionTurnRole::Assistant, format!("arc turn {i}"));
        }
        let recent = store.recent_conversation();
        // MAX_TURNS_IN_PROMPT turns = 12 user/arc pairs, i.e. turns 48..=59.
        assert_eq!(recent.matches("user:").count(), MAX_TURNS_IN_PROMPT / 2);
        assert_eq!(recent.matches("arc:").count(), MAX_TURNS_IN_PROMPT / 2);
        assert!(recent.contains("user turn 48"), "oldest kept turn missing");
        assert!(recent.contains("arc turn 59"), "newest turn missing");
        assert!(!recent.contains("user turn 47"), "turn older than the cap leaked in");
        assert!(!recent.contains("user turn 0"));
    }

    #[test]
    fn append_turn_creates_new_session_on_first_turn() {
        let store = tmp_store("first");
        let summary = store.append_turn(SessionTurnRole::User, "hello".into());
        assert_eq!(summary.total_turns, 1);
        assert_eq!(summary.turns.len(), 1);
        assert_eq!(summary.turns[0].text, "hello");
        let sessions = store.sessions_lock();
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn search_session_finds_turns() {
        let store = tmp_store("find");
        store.append_turn(SessionTurnRole::User, "open firefox".into());
        store.append_turn(SessionTurnRole::Assistant, "Done.".into());
        let summary = store.append_turn(SessionTurnRole::User, "open chromium".into());
        store.append_turn(SessionTurnRole::Assistant, "Done.".into());
        let id = summary.id.clone();
        let turns = store.search_session(&id, vec!["firefox"], 5).unwrap();
        assert_eq!(turns.len(), 1);
        assert!(turns[0].text.contains("firefox"));
        let turns = store.search_session(&id, vec!["chromium"], 5).unwrap();
        assert_eq!(turns.len(), 1);
        assert!(turns[0].text.contains("chromium"));
    }

    #[test]
    fn search_session_returns_none_for_missing_id() {
        let store = tmp_store("missing");
        assert!(store.search_session("does-not-exist", vec!["x"], 5).is_none());
    }

    #[test]
    fn search_is_case_insensitive() {
        let store = tmp_store("case");
        store.remember("I use Arch Linux".into(), vec![]);
        for q in ["arch", "ARCH", "linux"] {
            assert_eq!(store.search(vec![q]).len(), 1, "{q}");
        }
    }
}