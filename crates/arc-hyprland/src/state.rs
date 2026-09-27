//! Live desktop state kept current from Hyprland events, plus resolution of
//! spoken window references ("this", "that", "firefox", "the terminal").
//!
//! Strategy: on any relevant event, re-query `clients`/`workspaces`/
//! `monitors` (≈1 ms each over the socket) after a short debounce, rather
//! than maintaining an incremental model that can drift. No polling: when
//! nothing happens on the desktop, nothing runs.

use crate::events::{EventStream, HyprEvent};
use crate::types::{Client, Monitor, Workspace};
use crate::{HyprError, Hyprland};
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{RwLock, watch};

/// Window classes that belong to Arc itself (never "this window").
pub const ARC_CLASSES: &[&str] = &["arc", "arc-ui", "dev.arc.ui", "arc-probe"];

#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub connected: bool,
    pub clients: Vec<Client>,
    pub workspaces: Vec<Workspace>,
    pub monitors: Vec<Monitor>,
    pub active_workspace: Option<Workspace>,
    pub active_window: Option<String>,
    /// Unix seconds of the last refresh.
    pub updated_at: u64,
    pub error: Option<String>,
}

impl Snapshot {
    pub fn client(&self, address: &str) -> Option<&Client> {
        self.clients.iter().find(|c| c.address == address)
    }

    /// User-visible windows, most recently focused first.
    pub fn windows_by_recency(&self) -> Vec<&Client> {
        let mut v: Vec<&Client> =
            self.clients.iter().filter(|c| c.mapped && !is_arc(c) && !c.workspace.is_special()).collect();
        v.sort_by_key(|c| if c.focus_history_id < 0 { i64::MAX } else { c.focus_history_id });
        v
    }

    pub fn active_client(&self) -> Option<&Client> {
        self.active_window.as_deref().and_then(|a| self.client(a)).filter(|c| !is_arc(c))
    }

    /// Resolve a spoken reference to a window.
    ///
    /// * "", "this", "it", "current", "active", "focused", "here" → the focused
    ///   window (or, if Arc's own UI has focus, the most recent other window).
    /// * "that", "previous", "last", "other" → the window focused before it.
    /// * anything else → fuzzy match on app class, then title. Ties prefer
    ///   the current workspace, then the most recently focused.
    pub fn resolve_window(&self, reference: &str) -> Result<&Client, ResolveError> {
        let r = normalize_ref(reference);
        let recent = self.windows_by_recency();
        if recent.is_empty() {
            return Err(ResolveError::NoWindows);
        }
        match r.as_str() {
            "" | "this" | "it" | "current" | "active" | "focused" | "here" | "this window" | "the window"
            | "current window" | "active window" | "focused window" => {
                self.active_client().or_else(|| recent.first().copied()).ok_or(ResolveError::NoWindows)
            }
            "that" | "previous" | "last" | "other" | "that window" | "previous window" | "last window"
            | "other window" => {
                let active = self.active_client().map(|c| c.address.as_str());
                recent
                    .iter()
                    .find(|c| Some(c.address.as_str()) != active)
                    .copied()
                    .or_else(|| recent.first().copied())
                    .ok_or(ResolveError::NoWindows)
            }
            q => {
                let q = expand_role(q);
                let ws = self.active_workspace.as_ref().map(|w| w.id);
                let score = |c: &Client| -> u32 {
                    let class = c.class.to_lowercase();
                    let init = c.initial_class.to_lowercase();
                    let app = c.app_name().to_lowercase();
                    let title = c.title.to_lowercase();
                    let mut best = 0;
                    for q in &q {
                        let s = if class == *q || app == *q || init == *q {
                            100
                        } else if class.contains(q.as_str())
                            || app.contains(q.as_str())
                            || init.contains(q.as_str())
                        {
                            70
                        } else if title.contains(q.as_str()) {
                            40
                        } else {
                            0
                        };
                        best = best.max(s);
                    }
                    best
                };
                let mut cands: Vec<(u32, &Client)> =
                    recent.iter().map(|c| (score(c), *c)).filter(|(s, _)| *s > 0).collect();
                if cands.is_empty() {
                    return Err(ResolveError::NotFound(reference.trim().to_string()));
                }
                cands.sort_by_key(|(s, c)| {
                    (std::cmp::Reverse(*s), Some(c.workspace.id) != ws, c.focus_history_id.max(0))
                });
                Ok(cands[0].1)
            }
        }
    }

    pub fn focused_monitor(&self) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.focused).or(self.monitors.first())
    }

    /// Resolve "left monitor", "HDMI-A-1", "second monitor", "other monitor".
    pub fn resolve_monitor(&self, reference: &str) -> Option<&Monitor> {
        let r = reference.trim().to_lowercase();
        let r = r.trim_end_matches(" monitor").trim_end_matches(" screen").trim_end_matches(" display");
        let r = r.trim_start_matches("the ").trim();
        let mut sorted: Vec<&Monitor> = self.monitors.iter().filter(|m| !m.disabled).collect();
        sorted.sort_by_key(|m| (m.x, m.y));
        if let Some(m) = self.monitors.iter().find(|m| m.name.to_lowercase() == r) {
            return Some(m);
        }
        match r {
            "left" | "leftmost" | "first" | "1" | "primary" | "main" => sorted.first().copied(),
            "right" | "rightmost" | "last" => sorted.last().copied(),
            "second" | "2" => sorted.get(1).copied(),
            "third" | "3" => sorted.get(2).copied(),
            "other" | "next" => sorted.iter().find(|m| !m.focused).copied(),
            "this" | "current" | "focused" => self.focused_monitor(),
            _ => self
                .monitors
                .iter()
                .find(|m| m.description.to_lowercase().contains(r) || m.model.to_lowercase().contains(r)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NoWindows,
    NotFound(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NoWindows => f.write_str("there are no open windows"),
            ResolveError::NotFound(q) => write!(f, "I don't see a window matching \"{q}\""),
        }
    }
}

pub fn is_arc(c: &Client) -> bool {
    ARC_CLASSES.contains(&c.class.as_str()) || ARC_CLASSES.contains(&c.initial_class.as_str())
}

fn normalize_ref(s: &str) -> String {
    let s = s.trim().to_lowercase();
    let s = s.trim_end_matches(['.', '?', '!']);
    let s = s.strip_prefix("the ").unwrap_or(s);
    let s =
        s.strip_suffix(" window").filter(|x| !x.is_empty() && !matches!(*x, "this" | "that")).unwrap_or(s);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Map generic roles to likely window classes.
fn expand_role(q: &str) -> Vec<String> {
    let extra: &[&str] = match q {
        "browser" | "web browser" | "internet" => {
            &["chromium", "firefox", "brave", "chrome", "zen", "librewolf"]
        }
        "terminal" | "shell" | "console" => &["kitty", "alacritty", "ghostty", "foot", "terminal"],
        "editor" | "code editor" | "code" => &["code", "nvim", "zed", "cursor", "neovide"],
        "files" | "file manager" | "file browser" => &["nautilus", "thunar", "dolphin", "files"],
        "music" | "music player" => &["spotify", "mpv", "rhythmbox"],
        "chat" => &["discord", "vesktop", "signal", "telegram", "slack"],
        _ => &[],
    };
    let mut v = vec![q.to_string()];
    v.extend(extra.iter().map(|s| s.to_string()));
    v
}

// ---------------------------------------------------------------------------
// Tracker
// ---------------------------------------------------------------------------

/// Owns the snapshot and keeps it current. Cheap to clone.
#[derive(Clone)]
pub struct DesktopTracker {
    hypr: Option<Hyprland>,
    snap: Arc<RwLock<Snapshot>>,
    tx: watch::Sender<u64>,
}

impl DesktopTracker {
    /// Create; never fails. If Hyprland isn't reachable the snapshot says so
    /// and the rest of Arc keeps working.
    pub fn new(hypr: Option<Hyprland>) -> Self {
        let (tx, _) = watch::channel(0);
        Self { hypr, snap: Arc::new(RwLock::new(Snapshot::default())), tx }
    }

    pub fn hyprland(&self) -> Option<&Hyprland> {
        self.hypr.as_ref()
    }

    pub async fn snapshot(&self) -> Snapshot {
        self.snap.read().await.clone()
    }

    /// Notified (with a generation counter) after each refresh.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.tx.subscribe()
    }

    pub async fn refresh(&self) -> Result<(), HyprError> {
        let Some(h) = &self.hypr else {
            let mut s = self.snap.write().await;
            s.connected = false;
            s.error = Some("Hyprland integration is unavailable".into());
            return Err(HyprError::Unavailable("not running under Hyprland".into()));
        };
        let res = async {
            let (clients, workspaces, monitors, active_ws, active_win) = tokio::try_join!(
                h.clients(),
                h.workspaces(),
                h.monitors(),
                h.active_workspace(),
                h.active_window()
            )?;
            Ok::<_, HyprError>((clients, workspaces, monitors, active_ws, active_win))
        }
        .await;
        let mut s = self.snap.write().await;
        match res {
            Ok((clients, mut workspaces, monitors, active_ws, active_win)) => {
                workspaces.sort_by_key(|w| w.id);
                *s = Snapshot {
                    connected: true,
                    clients,
                    workspaces,
                    monitors,
                    active_workspace: Some(active_ws),
                    active_window: active_win.map(|c| c.address),
                    updated_at: SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                    error: None,
                };
                drop(s);
                self.tx.send_modify(|g| *g += 1);
                Ok(())
            }
            Err(e) => {
                s.connected = false;
                s.error = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// Run forever: initial refresh, then refresh after each burst of
    /// events (50 ms debounce). Callers spawn this as a task.
    pub async fn run(self, on_event: impl Fn(&HyprEvent) + Send + 'static) {
        let Some(h) = self.hypr.clone() else {
            let _ = self.refresh().await;
            return;
        };
        if let Err(e) = self.refresh().await {
            tracing::warn!(error = %e, "initial Hyprland refresh failed");
        }
        let mut stream = EventStream::new(h.event_socket());
        loop {
            let ev = stream.next().await;
            on_event(&ev);
            if !ev.affects_clients() {
                continue;
            }
            // Coalesce the burst that usually accompanies one user action.
            let deadline = tokio::time::sleep(Duration::from_millis(50));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    ev = stream.next() => on_event(&ev),
                }
            }
            if let Err(e) = self.refresh().await {
                tracing::debug!(error = %e, "desktop refresh failed");
            }
        }
    }

    /// Test helper / offline use.
    pub async fn set_snapshot(&self, s: Snapshot) {
        *self.snap.write().await = s;
        self.tx.send_modify(|g| *g += 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::WorkspaceRef;

    fn win(addr: &str, class: &str, title: &str, ws: i64, hist: i64) -> Client {
        Client {
            address: addr.into(),
            mapped: true,
            class: class.into(),
            initial_class: class.into(),
            title: title.into(),
            workspace: WorkspaceRef { id: ws, name: ws.to_string() },
            focus_history_id: hist,
            ..Default::default()
        }
    }

    fn snap() -> Snapshot {
        Snapshot {
            connected: true,
            clients: vec![
                win("0x1", "chromium", "YouTube - Chromium", 4, 1),
                win("0x2", "kitty", "nvim ~/Projects/arc", 2, 0),
                win("0x3", "kitty", "btop", 5, 3),
                win("0x4", "arc-ui", "Arc", 2, 2),
                win("0x5", "org.gnome.Nautilus", "Downloads", 3, 4),
                Client {
                    workspace: WorkspaceRef { id: -98, name: "special:scratchpad".into() },
                    ..win("0x6", "spotify", "Spotify", -98, 5)
                },
            ],
            active_workspace: Some(Workspace { id: 2, name: "2".into(), ..Default::default() }),
            active_window: Some("0x2".into()),
            ..Default::default()
        }
    }

    #[test]
    fn this_and_that() {
        let s = snap();
        assert_eq!(s.resolve_window("this").unwrap().address, "0x2");
        assert_eq!(s.resolve_window("this window").unwrap().address, "0x2");
        assert_eq!(s.resolve_window("").unwrap().address, "0x2");
        assert_eq!(s.resolve_window("that").unwrap().address, "0x1");
        assert_eq!(s.resolve_window("the previous window").unwrap().address, "0x1");
    }

    #[test]
    fn arc_ui_focused_falls_back_to_last_real_window() {
        let mut s = snap();
        s.active_window = Some("0x4".into());
        assert_eq!(s.resolve_window("this").unwrap().address, "0x2");
    }

    #[test]
    fn by_name_and_role() {
        let s = snap();
        assert_eq!(s.resolve_window("chromium").unwrap().address, "0x1");
        assert_eq!(s.resolve_window("the browser").unwrap().address, "0x1");
        assert_eq!(s.resolve_window("nautilus").unwrap().address, "0x5");
        assert_eq!(s.resolve_window("file manager").unwrap().address, "0x5");
        // Two kitty windows: prefer current workspace (2).
        assert_eq!(s.resolve_window("kitty").unwrap().address, "0x2");
        assert_eq!(s.resolve_window("terminal").unwrap().address, "0x2");
        // Title match.
        assert_eq!(s.resolve_window("btop").unwrap().address, "0x3");
        assert_eq!(s.resolve_window("youtube").unwrap().address, "0x1");
        // Special-workspace windows are not candidates.
        assert!(matches!(s.resolve_window("spotify"), Err(ResolveError::NotFound(_))));
        assert!(matches!(s.resolve_window("firefox"), Err(ResolveError::NotFound(_))));
    }

    #[test]
    fn empty_desktop() {
        let s = Snapshot::default();
        assert_eq!(s.resolve_window("this"), Err(ResolveError::NoWindows));
    }

    #[test]
    fn monitors() {
        let mut s = snap();
        s.monitors = vec![
            Monitor { id: 1, name: "HDMI-A-1".into(), x: 1920, focused: true, ..Default::default() },
            Monitor {
                id: 0,
                name: "DP-1".into(),
                x: 0,
                description: "Dell U2419".into(),
                ..Default::default()
            },
        ];
        assert_eq!(s.resolve_monitor("left monitor").unwrap().name, "DP-1");
        assert_eq!(s.resolve_monitor("the right screen").unwrap().name, "HDMI-A-1");
        assert_eq!(s.resolve_monitor("other").unwrap().name, "DP-1");
        assert_eq!(s.resolve_monitor("dell").unwrap().name, "DP-1");
        assert_eq!(s.resolve_monitor("hdmi-a-1").unwrap().name, "HDMI-A-1");
        assert!(s.resolve_monitor("tv").is_none());
    }
}
