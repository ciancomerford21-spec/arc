//! Append-only audit trail of everything Arc decides and does.
//!
//! `~/.local/state/arc/audit.jsonl`: one JSON object per line, file mode
//! 0600, directory 0700. Arguments and details pass through
//! [`crate::redact`] before they are written. The file is rotated to
//! `audit.jsonl.1` when it exceeds the size limit (one generation kept).

use crate::redact::{redact_str, redact_value};
use arc_proto::RiskLevel;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    Startup,
    Shutdown,
    Request,
    ToolCall,
    Permission,
    Confirmation,
    Memory,
    Config,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditRecord {
    pub ts: String,
    pub kind: AuditKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Where the request came from: voice, text, cli, automation, ui.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<RiskLevel>,
    /// allow / confirm / deny / approved / rejected / expired …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    /// success / failed / denied / cancelled …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl AuditRecord {
    pub fn new(kind: AuditKind) -> Self {
        Self {
            ts: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
            kind,
            request_id: None,
            source: None,
            tool: None,
            args: Value::Null,
            risk: None,
            decision: None,
            outcome: None,
            duration_ms: None,
            detail: None,
        }
    }
    pub fn request(mut self, id: impl Into<String>) -> Self {
        self.request_id = Some(id.into());
        self
    }
    pub fn source(mut self, s: impl Into<String>) -> Self {
        self.source = Some(s.into());
        self
    }
    pub fn tool(mut self, t: impl Into<String>, args: &Value) -> Self {
        self.tool = Some(t.into());
        self.args = args.clone();
        self
    }
    pub fn risk(mut self, r: RiskLevel) -> Self {
        self.risk = Some(r);
        self
    }
    pub fn decision(mut self, d: impl Into<String>) -> Self {
        self.decision = Some(d.into());
        self
    }
    pub fn outcome(mut self, o: impl Into<String>) -> Self {
        self.outcome = Some(o.into());
        self
    }
    pub fn duration_ms(mut self, ms: u64) -> Self {
        self.duration_ms = Some(ms);
        self
    }
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }

    fn redacted(&self) -> Self {
        let mut r = self.clone();
        r.args = redact_value(&r.args);
        r.detail = r.detail.map(|d| redact_str(&d).into_owned());
        r
    }
}

pub struct AuditLog {
    path: PathBuf,
    max_bytes: u64,
    file: Mutex<Option<File>>,
}

impl AuditLog {
    /// Open (creating as needed) with owner-only permissions.
    pub fn open(path: impl Into<PathBuf>, max_bytes: u64) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
        let file = open_append(&path)?;
        Ok(Self { path, max_bytes, file: Mutex::new(Some(file)) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record. Errors are returned, never panicked on; callers
    /// usually log them via tracing and carry on (auditing must not take
    /// Arc down).
    pub fn record(&self, rec: &AuditRecord) -> std::io::Result<()> {
        let mut line = serde_json::to_string(&rec.redacted()).map_err(std::io::Error::other)?;
        line.push('\n');
        let mut guard = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(meta) = fs::metadata(&self.path)
            && meta.len() + line.len() as u64 > self.max_bytes
        {
            *guard = None;
            let rotated = self.path.with_extension("jsonl.1");
            let _ = fs::rename(&self.path, &rotated);
        }
        if guard.is_none() {
            *guard = Some(open_append(&self.path)?);
        }
        let f = guard.as_mut().expect("audit file open");
        f.write_all(line.as_bytes())?;
        f.flush()
    }

    /// Last `n` records (from the current file only), oldest first.
    /// Malformed lines are skipped.
    pub fn tail(&self, n: usize) -> std::io::Result<Vec<AuditRecord>> {
        read_tail(&self.path, n)
    }
}

pub fn read_tail(path: &Path, n: usize) -> std::io::Result<Vec<AuditRecord>> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut ring = std::collections::VecDeque::with_capacity(n.min(4096));
    for line in BufReader::new(f).lines() {
        let line = line?;
        if let Ok(rec) = serde_json::from_str::<AuditRecord>(&line) {
            if ring.len() == n {
                ring.pop_front();
            }
            if n > 0 {
                ring.push_back(rec);
            }
        }
    }
    Ok(ring.into_iter().collect())
}

fn open_append(path: &Path) -> std::io::Result<File> {
    let f = OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
    // Tighten pre-existing files too.
    let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn append_and_tail_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path().join("state/audit.jsonl"), 1 << 20).unwrap();
        for i in 0..5 {
            log.record(
                &AuditRecord::new(AuditKind::ToolCall)
                    .request(format!("r{i}"))
                    .source("cli")
                    .tool("hyprland.switch_workspace", &json!({"workspace": i}))
                    .risk(RiskLevel::Safe)
                    .decision("allow")
                    .outcome("success")
                    .duration_ms(3),
            )
            .unwrap();
        }
        let t = log.tail(3).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].request_id.as_deref(), Some("r2"));
        assert_eq!(t[2].args, json!({"workspace": 4}));
        assert!(log.tail(0).unwrap().is_empty());
    }

    #[test]
    fn permissions_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s/audit.jsonl");
        let log = AuditLog::open(&p, 1 << 20).unwrap();
        log.record(&AuditRecord::new(AuditKind::Startup)).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(p.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn secrets_never_reach_disk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&p, 1 << 20).unwrap();
        log.record(
            &AuditRecord::new(AuditKind::ToolCall)
                .tool("web.request", &json!({"api_key": "hunter2hunter2", "q": "weather"}))
                .detail("used sk-ant-api03-AAAABBBBCCCCDDDDEEEE"),
        )
        .unwrap();
        let raw = fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("hunter2"));
        assert!(!raw.contains("sk-ant-api03"));
        assert!(raw.contains("weather"));
    }

    #[test]
    fn rotation_keeps_one_generation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&p, 400).unwrap();
        for _ in 0..20 {
            log.record(&AuditRecord::new(AuditKind::Request).detail("x".repeat(50))).unwrap();
        }
        assert!(fs::metadata(&p).unwrap().len() <= 400);
        assert!(p.with_extension("jsonl.1").exists());
        assert!(!log.tail(100).unwrap().is_empty());
    }

    #[test]
    fn malformed_lines_skipped_and_missing_file_ok() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("audit.jsonl");
        assert!(read_tail(&p, 10).unwrap().is_empty());
        fs::write(&p, "garbage\n{\"ts\":\"t\",\"kind\":\"startup\"}\n").unwrap();
        let t = read_tail(&p, 10).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, AuditKind::Startup);
    }
}
