//! Media players over MPRIS (D-Bus), no playerctl required.

use crate::{Result, SysError};
use serde::Serialize;
use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaAction {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
}

impl MediaAction {
    fn method(self) -> &'static str {
        match self {
            MediaAction::Play => "Play",
            MediaAction::Pause => "Pause",
            MediaAction::PlayPause => "PlayPause",
            MediaAction::Stop => "Stop",
            MediaAction::Next => "Next",
            MediaAction::Previous => "Previous",
        }
    }
    pub fn past_tense(self) -> &'static str {
        match self {
            MediaAction::Play => "Playing",
            MediaAction::Pause => "Paused",
            MediaAction::PlayPause => "Toggled playback",
            MediaAction::Stop => "Stopped",
            MediaAction::Next => "Skipped to the next track",
            MediaAction::Previous => "Went back a track",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlayerInfo {
    pub bus_name: String,
    /// Short name, e.g. "spotify", "chromium".
    pub name: String,
    pub status: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub volume: Option<f64>,
}

impl PlayerInfo {
    pub fn describe(&self) -> String {
        match (&self.title, &self.artist) {
            (Some(t), Some(a)) if !t.is_empty() && !a.is_empty() => format!("{t} by {a}"),
            (Some(t), _) if !t.is_empty() => t.clone(),
            _ => format!("{} ({})", self.name, self.status.to_lowercase()),
        }
    }
}

pub struct Media {
    conn: zbus::Connection,
}

impl Media {
    pub async fn connect() -> Result<Self> {
        Ok(Self { conn: zbus::Connection::session().await? })
    }

    pub fn with_connection(conn: zbus::Connection) -> Self {
        Self { conn }
    }

    pub async fn players(&self) -> Result<Vec<PlayerInfo>> {
        let dbus = zbus::fdo::DBusProxy::new(&self.conn).await?;
        let names = dbus.list_names().await?;
        let mut out = vec![];
        for n in names.iter().map(|n| n.as_str()).filter(|n| n.starts_with(PREFIX)) {
            match self.info(n).await {
                Ok(i) => out.push(i),
                Err(e) => tracing::debug!(player = n, error = %e, "skipping MPRIS player"),
            }
        }
        Ok(out)
    }

    async fn info(&self, bus: &str) -> Result<PlayerInfo> {
        let props =
            zbus::fdo::PropertiesProxy::builder(&self.conn).destination(bus)?.path(PATH)?.build().await?;
        let iface =
            zbus::names::InterfaceName::try_from(PLAYER_IFACE).map_err(|e| SysError::DBus(e.to_string()))?;
        let all: HashMap<String, OwnedValue> = props.get_all(iface).await?;
        let status = all
            .get("PlaybackStatus")
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .unwrap_or_else(|| "Unknown".into());
        let volume = all.get("Volume").and_then(|v| f64::try_from(v).ok());
        let (mut title, mut artist, mut album) = (None, None, None);
        if let Some(md) = all.get("Metadata")
            && let Ok(md) = HashMap::<String, OwnedValue>::try_from(
                md.try_clone().map_err(|e| SysError::DBus(e.to_string()))?,
            )
        {
            title = md.get("xesam:title").and_then(|v| String::try_from(v.try_clone().ok()?).ok());
            album = md.get("xesam:album").and_then(|v| String::try_from(v.try_clone().ok()?).ok());
            artist = md
                .get("xesam:artist")
                .and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok())
                .map(|a| a.join(", "));
        }
        Ok(PlayerInfo {
            bus_name: bus.to_string(),
            name: short_name(bus),
            status,
            title,
            artist,
            album,
            volume,
        })
    }

    /// Choose the player to control: by name if given, else the one that is
    /// Playing, else Paused, else any.
    pub async fn pick(&self, name: Option<&str>) -> Result<PlayerInfo> {
        let players = self.players().await?;
        if players.is_empty() {
            return Err(SysError::NotFound("no media player is running".into()));
        }
        choose(players, name)
    }

    pub async fn control(&self, action: MediaAction, name: Option<&str>) -> Result<PlayerInfo> {
        let p = self.pick(name).await?;
        let proxy = zbus::Proxy::new(&self.conn, p.bus_name.as_str(), PATH, PLAYER_IFACE).await?;
        proxy.call_method(action.method(), &()).await.map_err(|e| SysError::Command {
            what: format!("{} {}", p.name, action.method()),
            detail: e.to_string(),
        })?;
        // Give the player a moment to publish its new state.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        Ok(self.info(&p.bus_name).await.unwrap_or(p))
    }

    /// Player volume 0–100 (not the system volume).
    pub async fn set_volume(&self, percent: u32, name: Option<&str>) -> Result<PlayerInfo> {
        let p = self.pick(name).await?;
        let proxy = zbus::Proxy::new(&self.conn, p.bus_name.as_str(), PATH, PLAYER_IFACE).await?;
        proxy
            .set_property("Volume", percent.min(100) as f64 / 100.0)
            .await
            .map_err(|e| SysError::DBus(e.to_string()))?;
        Ok(self.info(&p.bus_name).await.unwrap_or(p))
    }
}

fn short_name(bus: &str) -> String {
    let s = bus.strip_prefix(PREFIX).unwrap_or(bus);
    s.split('.').next().unwrap_or(s).to_string()
}

pub fn choose(mut players: Vec<PlayerInfo>, name: Option<&str>) -> Result<PlayerInfo> {
    if let Some(n) = name.map(|n| n.trim().to_lowercase()).filter(|n| !n.is_empty()) {
        return players
            .into_iter()
            .find(|p| p.name.to_lowercase().contains(&n) || p.bus_name.to_lowercase().contains(&n))
            .ok_or_else(|| SysError::NotFound(format!("no media player named \"{n}\" is running")));
    }
    let rank = |s: &str| match s {
        "Playing" => 0,
        "Paused" => 1,
        _ => 2,
    };
    players.sort_by_key(|p| rank(&p.status));
    Ok(players.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str, status: &str) -> PlayerInfo {
        PlayerInfo {
            bus_name: format!("{PREFIX}{name}.instance1"),
            name: name.into(),
            status: status.into(),
            title: None,
            artist: None,
            album: None,
            volume: None,
        }
    }

    #[test]
    fn choose_prefers_playing_then_name() {
        let v = vec![p("chromium", "Paused"), p("spotify", "Playing"), p("mpv", "Stopped")];
        assert_eq!(choose(v.clone(), None).unwrap().name, "spotify");
        assert_eq!(choose(v.clone(), Some("chrom")).unwrap().name, "chromium");
        assert!(choose(v, Some("vlc")).is_err());
        assert_eq!(short_name("org.mpris.MediaPlayer2.chromium.instance23479"), "chromium");
    }

    #[test]
    fn describe() {
        let mut x = p("spotify", "Playing");
        x.title = Some("Song".into());
        x.artist = Some("Band".into());
        assert_eq!(x.describe(), "Song by Band");
        assert_eq!(p("mpv", "Paused").describe(), "mpv (paused)");
    }
}
