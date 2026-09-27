//! Hyprland event stream (`.socket2.sock`).
//!
//! Lines look like `openwindow>>60731801fe30,special:x,arc-probe,kitty`.
//! [`EventStream`] reconnects with backoff if Hyprland restarts, so a
//! consumer can simply loop on [`EventStream::next`].

use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedReadHalf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HyprEvent {
    /// Active workspace changed (`workspacev2>>id,name`).
    Workspace {
        id: i64,
        name: String,
    },
    /// Focused monitor changed (`focusedmonv2>>mon,wsid`).
    FocusedMonitor {
        monitor: String,
        workspace_id: i64,
    },
    /// Focused window changed (`activewindowv2>>addr`, empty = none).
    ActiveWindow {
        address: Option<String>,
    },
    WindowTitle {
        address: String,
        title: String,
    },
    OpenWindow {
        address: String,
        workspace: String,
        class: String,
        title: String,
    },
    CloseWindow {
        address: String,
    },
    MoveWindow {
        address: String,
        workspace_id: i64,
        workspace: String,
    },
    Floating {
        address: String,
        floating: bool,
    },
    Fullscreen {
        on: bool,
    },
    CreateWorkspace {
        id: i64,
        name: String,
    },
    DestroyWorkspace {
        id: i64,
        name: String,
    },
    MonitorAdded {
        name: String,
    },
    MonitorRemoved {
        name: String,
    },
    /// Hyprland config reloaded.
    ConfigReloaded,
    /// Any other event, kept verbatim.
    Other {
        name: String,
        data: String,
    },
}

impl HyprEvent {
    /// Whether this event can change the window list (cheap full refresh).
    pub fn affects_clients(&self) -> bool {
        !matches!(self, HyprEvent::Other { .. } | HyprEvent::ConfigReloaded)
    }
}

fn addr(s: &str) -> String {
    let s = s.trim();
    if s.starts_with("0x") { s.to_string() } else { format!("0x{s}") }
}

/// Parse one socket2 line. Returns `None` for superseded v1 events whose v2
/// twin carries the same information (avoids double handling).
pub fn parse_line(line: &str) -> Option<HyprEvent> {
    let (name, data) = line.split_once(">>")?;
    let mut it = data.splitn(4, ',');
    let mut f = || it.next().unwrap_or("").to_string();
    let int = |s: &str| s.trim().parse::<i64>().unwrap_or(0);
    Some(match name {
        "workspacev2" => {
            let (id, n) = (f(), f());
            HyprEvent::Workspace { id: int(&id), name: n }
        }
        "focusedmonv2" => {
            let (m, id) = (f(), f());
            HyprEvent::FocusedMonitor { monitor: m, workspace_id: int(&id) }
        }
        "activewindowv2" => {
            let a = data.trim();
            HyprEvent::ActiveWindow { address: if a.is_empty() || a == "," { None } else { Some(addr(a)) } }
        }
        "windowtitlev2" => {
            let (a, rest) = data.split_once(',').unwrap_or((data, ""));
            HyprEvent::WindowTitle { address: addr(a), title: rest.to_string() }
        }
        "openwindow" => {
            let (a, ws, class) = (f(), f(), f());
            HyprEvent::OpenWindow { address: addr(&a), workspace: ws, class, title: f() }
        }
        "closewindow" => HyprEvent::CloseWindow { address: addr(data) },
        "movewindowv2" => {
            let (a, id, ws) = (f(), f(), f());
            HyprEvent::MoveWindow { address: addr(&a), workspace_id: int(&id), workspace: ws }
        }
        "changefloatingmode" => {
            let (a, fl) = (f(), f());
            HyprEvent::Floating { address: addr(&a), floating: fl.trim() == "1" }
        }
        "fullscreen" => HyprEvent::Fullscreen { on: data.trim() == "1" },
        "createworkspacev2" => {
            let (id, n) = (f(), f());
            HyprEvent::CreateWorkspace { id: int(&id), name: n }
        }
        "destroyworkspacev2" => {
            let (id, n) = (f(), f());
            HyprEvent::DestroyWorkspace { id: int(&id), name: n }
        }
        "monitoraddedv2" => {
            let (_id, n) = (f(), f());
            HyprEvent::MonitorAdded { name: n }
        }
        "monitorremovedv2" => {
            let (_id, n) = (f(), f());
            HyprEvent::MonitorRemoved { name: n }
        }
        "configreloaded" => HyprEvent::ConfigReloaded,
        "workspace" | "focusedmon" | "activewindow" | "windowtitle" | "movewindow" | "createworkspace"
        | "destroyworkspace" | "monitoradded" | "monitorremoved" => return None,
        _ => HyprEvent::Other { name: name.to_string(), data: data.to_string() },
    })
}

pub struct EventStream {
    path: PathBuf,
    lines: Option<Lines<BufReader<OwnedReadHalf>>>,
    backoff: Duration,
}

impl EventStream {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lines: None, backoff: Duration::from_millis(250) }
    }

    /// Next event. Reconnects forever on failure (backoff up to 5 s); only
    /// returns once an event arrives.
    pub async fn next(&mut self) -> HyprEvent {
        loop {
            if self.lines.is_none() {
                match UnixStream::connect(&self.path).await {
                    Ok(s) => {
                        let (r, _w) = s.into_split();
                        self.lines = Some(BufReader::new(r).lines());
                        self.backoff = Duration::from_millis(250);
                        tracing::info!(path = %self.path.display(), "connected to Hyprland event socket");
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "Hyprland event socket unavailable; retrying");
                        tokio::time::sleep(self.backoff).await;
                        self.backoff = (self.backoff * 2).min(Duration::from_secs(5));
                        continue;
                    }
                }
            }
            match self.lines.as_mut().expect("connected").next_line().await {
                Ok(Some(line)) => {
                    if let Some(ev) = parse_line(&line) {
                        return ev;
                    }
                }
                Ok(None) | Err(_) => {
                    tracing::warn!("Hyprland event socket closed; reconnecting");
                    self.lines = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_captured_lines() {
        assert_eq!(
            parse_line("openwindow>>60731801fe30,special:arcprobe,arc-probe,kitty"),
            Some(HyprEvent::OpenWindow {
                address: "0x60731801fe30".into(),
                workspace: "special:arcprobe".into(),
                class: "arc-probe".into(),
                title: "kitty".into()
            })
        );
        assert_eq!(
            parse_line("closewindow>>60731801fe30"),
            Some(HyprEvent::CloseWindow { address: "0x60731801fe30".into() })
        );
        assert_eq!(
            parse_line("windowtitlev2>>60731801fe30,a, title, with commas"),
            Some(HyprEvent::WindowTitle {
                address: "0x60731801fe30".into(),
                title: "a, title, with commas".into()
            })
        );
        assert_eq!(parse_line("workspacev2>>3,3"), Some(HyprEvent::Workspace { id: 3, name: "3".into() }));
        assert_eq!(parse_line("activewindowv2>>"), Some(HyprEvent::ActiveWindow { address: None }));
        assert_eq!(parse_line("activewindow>>chromium,IdleArc"), None);
        assert_eq!(
            parse_line("createworkspacev2>>-98,special:arcprobe"),
            Some(HyprEvent::CreateWorkspace { id: -98, name: "special:arcprobe".into() })
        );
        assert!(matches!(parse_line("screencast>>1,0"), Some(HyprEvent::Other { .. })));
        assert_eq!(parse_line("garbage"), None);
    }

    #[tokio::test]
    async fn stream_reads_and_reconnects() {
        use tokio::io::AsyncWriteExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".socket2.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            for batch in [&b"activewindow>>x,y\nworkspacev2>>2,2\n"[..], b"closewindow>>abc\n"] {
                let (mut s, _) = listener.accept().await.unwrap();
                s.write_all(batch).await.unwrap();
                // dropping `s` closes the connection -> client reconnects
            }
        });
        let mut es = EventStream::new(path);
        assert_eq!(es.next().await, HyprEvent::Workspace { id: 2, name: "2".into() });
        assert_eq!(es.next().await, HyprEvent::CloseWindow { address: "0xabc".into() });
        server.await.unwrap();
    }
}
