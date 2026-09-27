//! Typed views of Hyprland's JSON replies. Every field has a default so
//! additions/removals across Hyprland versions don't break parsing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WorkspaceRef {
    pub id: i64,
    pub name: String,
}

impl WorkspaceRef {
    pub fn is_special(&self) -> bool {
        self.id < 0 || self.name.starts_with("special:")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Client {
    pub address: String,
    pub mapped: bool,
    pub hidden: bool,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub workspace: WorkspaceRef,
    pub floating: bool,
    pub pinned: bool,
    pub monitor: i64,
    pub class: String,
    pub title: String,
    pub initial_class: String,
    pub initial_title: String,
    pub pid: i64,
    pub xwayland: bool,
    /// 0 none, 1 maximized, 2 fullscreen.
    pub fullscreen: i64,
    #[serde(rename = "focusHistoryID")]
    pub focus_history_id: i64,
}

impl Client {
    /// Human-friendly application name: class without reverse-DNS prefix.
    pub fn app_name(&self) -> String {
        let c = if self.class.is_empty() { &self.initial_class } else { &self.class };
        let short = c.rsplit('.').next().unwrap_or(c);
        let mut chars = short.chars();
        match chars.next() {
            Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
            None => "Unknown".into(),
        }
    }
    pub fn selector(&self) -> String {
        format!("address:{}", self.address)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Workspace {
    pub id: i64,
    pub name: String,
    pub monitor: String,
    #[serde(rename = "monitorID")]
    pub monitor_id: i64,
    pub windows: i64,
    pub hasfullscreen: bool,
    pub lastwindow: String,
    pub lastwindowtitle: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub make: String,
    pub model: String,
    pub width: i64,
    pub height: i64,
    pub refresh_rate: f64,
    pub x: i64,
    pub y: i64,
    pub scale: f64,
    pub focused: bool,
    pub active_workspace: WorkspaceRef,
    pub special_workspace: WorkspaceRef,
    pub disabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_client_json() {
        let raw = r#"{"address":"0x6073182392c0","mapped":true,"hidden":false,"at":[0,26],"size":[1920,1054],
          "workspace":{"id":4,"name":"4"},"floating":false,"monitor":1,"class":"chromium","title":"YouTube - Chromium",
          "initialClass":"chromium","initialTitle":"Chromium","pid":1234,"xwayland":false,"pinned":false,
          "fullscreen":0,"fullscreenClient":0,"focusHistoryID":2,"tags":[],"stableId":"x","unknownFutureField":1}"#;
        let c: Client = serde_json::from_str(raw).unwrap();
        assert_eq!(c.workspace.id, 4);
        assert_eq!(c.focus_history_id, 2);
        assert_eq!(c.app_name(), "Chromium");
        assert_eq!(c.selector(), "address:0x6073182392c0");
    }

    #[test]
    fn app_name_strips_reverse_dns() {
        let c = Client { class: "org.gnome.Nautilus".into(), ..Default::default() };
        assert_eq!(c.app_name(), "Nautilus");
        let c = Client { class: "".into(), initial_class: "kitty".into(), ..Default::default() };
        assert_eq!(c.app_name(), "Kitty");
    }

    #[test]
    fn parses_monitor_and_workspace() {
        let m: Monitor = serde_json::from_str(r#"{"id":1,"name":"HDMI-A-1","width":1920,"height":1080,"x":0,"y":0,
            "scale":1,"focused":true,"activeWorkspace":{"id":5,"name":"5"},"specialWorkspace":{"id":0,"name":""},"refreshRate":60.0}"#).unwrap();
        assert_eq!(m.active_workspace.name, "5");
        let w: Workspace = serde_json::from_str(
            r#"{"id":4,"name":"4","monitor":"HDMI-A-1","monitorID":1,"windows":1,
            "hasfullscreen":false,"lastwindow":"0x1","lastwindowtitle":"t","ispersistent":false}"#,
        )
        .unwrap();
        assert_eq!(w.monitor_id, 1);
        assert!(WorkspaceRef { id: -98, name: "special:x".into() }.is_special());
    }
}
