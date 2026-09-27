//! System integration for Arc. Every module degrades gracefully: a missing
//! command, bus name or sensor returns a descriptive [`SysError`] instead of
//! panicking, so one broken subsystem never takes the others down.

pub mod apps;
pub mod audio;
pub mod media;
pub mod monitor;
pub mod network;
pub mod notify;
pub mod power;

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum SysError {
    #[error("{0} is not available on this system")]
    Missing(String),
    #[error("{what} failed: {detail}")]
    Command { what: String, detail: String },
    #[error("{0}")]
    NotFound(String),
    #[error("D-Bus error: {0}")]
    DBus(String),
    #[error("{0} timed out")]
    Timeout(String),
    #[error("{0}")]
    Invalid(String),
}

impl From<zbus::Error> for SysError {
    fn from(e: zbus::Error) -> Self {
        SysError::DBus(e.to_string())
    }
}
impl From<zbus::fdo::Error> for SysError {
    fn from(e: zbus::fdo::Error) -> Self {
        SysError::DBus(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, SysError>;

/// Run a program (no shell) with a timeout and return trimmed stdout.
/// Non-zero exit becomes [`SysError::Command`] carrying stderr.
pub async fn run(prog: &str, args: &[&str], timeout: Duration) -> Result<String> {
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => return Err(SysError::Timeout(format!("{prog} {}", args.join(" ")))),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Err(SysError::Missing(prog.into())),
        Ok(Err(e)) => return Err(SysError::Command { what: prog.into(), detail: e.to_string() }),
        Ok(Ok(o)) => o,
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let err = if err.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { err };
        return Err(SysError::Command {
            what: format!("{prog} {}", args.join(" ")),
            detail: if err.is_empty() { format!("exit status {}", out.status) } else { err },
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn which(prog: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(prog)).find(|p| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    })
}

pub(crate) const SHORT: Duration = Duration::from_secs(3);

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_reports_missing_and_failure() {
        assert!(matches!(run("definitely-not-a-binary-arc", &[], SHORT).await, Err(SysError::Missing(_))));
        assert!(matches!(run("false", &[], SHORT).await, Err(SysError::Command { .. })));
        assert_eq!(run("echo", &["hi"], SHORT).await.unwrap(), "hi");
        assert!(matches!(run("sleep", &["5"], Duration::from_millis(100)).await, Err(SysError::Timeout(_))));
        assert!(which("sh").is_some());
    }
}
