//! Hyprland integration for Arc.
//!
//! * [`Hyprland`]: request/response over `.socket.sock` (`j/clients`,
//!   `dispatch …`). One short-lived connection per request, as hyprctl does.
//! * [`Dispatch`]: typed dispatcher calls rendered to Hyprland ≥ 0.55 Lua
//!   syntax (`hl.dsp.window.move({ … })`). Every window-level action targets an
//!   explicit `address:0x…` so Arc never acts on whatever happens to be focused
//!   by the time the command lands.
//! * [`events`]: `.socket2.sock` event stream (`name>>data`) parsed into
//!   [`events::HyprEvent`].
//! * [`state::DesktopState`]: a snapshot kept current by events.

pub mod dispatch;
pub mod events;
pub mod state;
pub mod types;

pub use dispatch::{Dispatch, FloatAction, FullscreenMode, WindowSel};
pub use types::{Client, Monitor, Workspace, WorkspaceRef};

use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Debug, thiserror::Error)]
pub enum HyprError {
    #[error("Hyprland integration is unavailable: {0}")]
    Unavailable(String),
    #[error("Hyprland IPC error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Hyprland IPC timed out")]
    Timeout,
    #[error("Hyprland rejected the command: {0}")]
    Rejected(String),
    #[error("unexpected Hyprland reply: {0}")]
    Parse(String),
}

pub type Result<T> = std::result::Result<T, HyprError>;

/// Locate the instance directory (`$XDG_RUNTIME_DIR/hypr/<signature>`).
///
/// Uses `$HYPRLAND_INSTANCE_SIGNATURE` when set. A systemd user service may
/// start before that variable is imported, so fall back to the most
/// recently modified instance directory that has a live socket.
pub fn instance_dir() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| HyprError::Unavailable("XDG_RUNTIME_DIR is not set".into()))?;
    let base = runtime.join("hypr");
    if let Some(sig) = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE") {
        let d = base.join(sig);
        if d.join(".socket.sock").exists() {
            return Ok(d);
        }
    }
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(&base).map_err(|_| HyprError::Unavailable("Hyprland is not running".into()))? {
        let Ok(e) = e else { continue };
        let sock = e.path().join(".socket.sock");
        if let Ok(m) = std::fs::metadata(&sock) {
            let t = m.modified().unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(bt, _)| t > *bt) {
                best = Some((t, e.path()));
            }
        }
    }
    best.map(|(_, p)| p).ok_or_else(|| HyprError::Unavailable("no Hyprland instance socket found".into()))
}

#[derive(Debug, Clone)]
pub struct Hyprland {
    dir: PathBuf,
    timeout: Duration,
}

impl Hyprland {
    pub fn connect() -> Result<Self> {
        Ok(Self::at(instance_dir()?))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), timeout: Duration::from_secs(2) }
    }

    pub fn instance_dir(&self) -> &Path {
        &self.dir
    }

    pub fn event_socket(&self) -> PathBuf {
        self.dir.join(".socket2.sock")
    }

    /// Raw request, returns the raw reply.
    pub async fn request(&self, cmd: &str) -> Result<String> {
        let fut = async {
            let mut s = UnixStream::connect(self.dir.join(".socket.sock"))
                .await
                .map_err(|e| HyprError::Unavailable(format!("cannot connect to Hyprland: {e}")))?;
            s.write_all(cmd.as_bytes()).await?;
            s.shutdown().await.ok();
            let mut buf = Vec::with_capacity(4096);
            s.read_to_end(&mut buf).await?;
            Ok::<_, HyprError>(String::from_utf8_lossy(&buf).into_owned())
        };
        tokio::time::timeout(self.timeout, fut).await.map_err(|_| HyprError::Timeout)?
    }

    /// JSON query, e.g. `json("clients")`.
    pub async fn json<T: serde::de::DeserializeOwned>(&self, what: &str) -> Result<T> {
        let raw = self.request(&format!("j/{what}")).await?;
        serde_json::from_str(&raw)
            .map_err(|e| HyprError::Parse(format!("{what}: {e}: {}", truncate(&raw, 160))))
    }

    pub async fn clients(&self) -> Result<Vec<Client>> {
        self.json("clients").await
    }
    pub async fn workspaces(&self) -> Result<Vec<Workspace>> {
        self.json("workspaces").await
    }
    pub async fn monitors(&self) -> Result<Vec<Monitor>> {
        self.json("monitors").await
    }
    pub async fn active_workspace(&self) -> Result<Workspace> {
        self.json("activeworkspace").await
    }
    /// `None` when no window is focused (Hyprland replies `{}`).
    pub async fn active_window(&self) -> Result<Option<Client>> {
        let v: serde_json::Value = self.json("activewindow").await?;
        if v.get("address").is_none() {
            return Ok(None);
        }
        serde_json::from_value(v).map(Some).map_err(|e| HyprError::Parse(e.to_string()))
    }
    pub async fn version(&self) -> Result<serde_json::Value> {
        self.json("version").await
    }

    /// Run a dispatcher. Hyprland answers `ok` on success; anything else is
    /// its error message. Note that Hyprland accepts some invalid argument
    /// values silently, so callers that need certainty re-query state.
    pub async fn dispatch(&self, d: &Dispatch) -> Result<()> {
        let lua = d.to_lua();
        tracing::debug!(dispatch = %lua, "hyprland dispatch");
        let reply = self.request(&format!("dispatch {lua}")).await?;
        let reply = reply.trim();
        if reply == "ok" { Ok(()) } else { Err(HyprError::Rejected(reply.to_string())) }
    }
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
