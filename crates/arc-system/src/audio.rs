//! Audio via WirePlumber's `wpctl` (PipeWire). Volume is expressed in percent.

use crate::{Result, SHORT, SysError, run};
use serde::Serialize;

pub const SINK: &str = "@DEFAULT_AUDIO_SINK@";
pub const SOURCE: &str = "@DEFAULT_AUDIO_SOURCE@";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Output,
    Input,
}

impl Channel {
    fn target(self) -> &'static str {
        match self {
            Channel::Output => SINK,
            Channel::Input => SOURCE,
        }
    }
    pub fn noun(self) -> &'static str {
        match self {
            Channel::Output => "volume",
            Channel::Input => "microphone",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VolumeState {
    pub percent: u32,
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Device {
    pub id: u32,
    pub name: String,
    pub default: bool,
    pub volume_percent: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Devices {
    pub outputs: Vec<Device>,
    pub inputs: Vec<Device>,
}

/// Parse `Volume: 0.40 [MUTED]`.
pub fn parse_volume(s: &str) -> Option<VolumeState> {
    let rest = s.trim().strip_prefix("Volume:")?.trim();
    let num = rest.split_whitespace().next()?.parse::<f64>().ok()?;
    Some(VolumeState { percent: (num * 100.0).round() as u32, muted: rest.contains("[MUTED]") })
}

pub async fn get(ch: Channel) -> Result<VolumeState> {
    let out = run("wpctl", &["get-volume", ch.target()], SHORT).await?;
    parse_volume(&out).ok_or_else(|| SysError::Command { what: "wpctl get-volume".into(), detail: out })
}

/// Set absolute volume in percent. Capped at `max` (default 100) to protect
/// ears and speakers; the result is read back and returned.
pub async fn set(ch: Channel, percent: u32, max: u32) -> Result<VolumeState> {
    let p = percent.min(max);
    let v = format!("{:.2}", p as f64 / 100.0);
    run("wpctl", &["set-volume", ch.target(), &v], SHORT).await?;
    if p > 0 {
        // Setting a volume implies you want to hear it.
        run("wpctl", &["set-mute", ch.target(), "0"], SHORT).await?;
    }
    get(ch).await
}

/// Relative change in percentage points.
pub async fn change(ch: Channel, delta: i32, max: u32) -> Result<VolumeState> {
    let cur = get(ch).await?;
    let target = (cur.percent as i32 + delta).clamp(0, max as i32) as u32;
    set(ch, target, max).await
}

/// `mute`: Some(true) mute, Some(false) unmute, None toggle.
pub async fn mute(ch: Channel, mute: Option<bool>) -> Result<VolumeState> {
    let arg = match mute {
        Some(true) => "1",
        Some(false) => "0",
        None => "toggle",
    };
    run("wpctl", &["set-mute", ch.target(), arg], SHORT).await?;
    get(ch).await
}

/// Parse the Audio section of `wpctl status`.
pub fn parse_status(s: &str) -> Devices {
    let mut d = Devices::default();
    let mut in_audio = false;
    let mut section = "";
    for line in s.lines() {
        let t = line.trim_start_matches(|c: char| c.is_whitespace() || "│├└─".contains(c)).trim_end();
        if line.starts_with("Audio") {
            in_audio = true;
            continue;
        }
        if in_audio && (line.starts_with("Video") || line.starts_with("Settings")) {
            break;
        }
        if !in_audio {
            continue;
        }
        if t.ends_with(':') {
            section = match t {
                "Sinks:" => "sinks",
                "Sources:" => "sources",
                _ => "",
            };
            continue;
        }
        if section.is_empty() || t.is_empty() {
            continue;
        }
        let default = t.starts_with('*');
        let t = t.trim_start_matches('*').trim();
        let Some((id, rest)) = t.split_once(". ") else { continue };
        let Ok(id) = id.trim().parse::<u32>() else { continue };
        let (name, vol) = match rest.rsplit_once(" [vol: ") {
            Some((n, v)) => {
                let v = v.trim_end_matches(']').split_whitespace().next().and_then(|x| x.parse::<f64>().ok());
                (n.trim().to_string(), v.map(|x| (x * 100.0).round() as u32))
            }
            None => (rest.trim().to_string(), None),
        };
        let dev = Device { id, name, default, volume_percent: vol };
        if section == "sinks" { d.outputs.push(dev) } else { d.inputs.push(dev) }
    }
    d
}

pub async fn devices() -> Result<Devices> {
    let out = run("wpctl", &["status", "-n"], SHORT).await.or(run("wpctl", &["status"], SHORT).await)?;
    let mut d = parse_status(&out);
    // `-n` shows node names instead of descriptions; prefer descriptions.
    if let Ok(pretty) = run("wpctl", &["status"], SHORT).await {
        let p = parse_status(&pretty);
        if p.outputs.len() == d.outputs.len() && p.inputs.len() == d.inputs.len() {
            d = p;
        }
    }
    Ok(d)
}

/// Pick a device by fuzzy name ("headset", "hdmi", "corsair") and make it the default.
pub async fn set_default(ch: Channel, query: &str) -> Result<Device> {
    let devs = devices().await?;
    let list = match ch {
        Channel::Output => devs.outputs,
        Channel::Input => devs.inputs,
    };
    let dev = best_device(&list, query)
        .ok_or_else(|| {
            SysError::NotFound(format!(
                "no audio {} matches \"{query}\"",
                if ch == Channel::Output { "output" } else { "input" }
            ))
        })?
        .clone();
    run("wpctl", &["set-default", &dev.id.to_string()], SHORT).await?;
    Ok(dev)
}

pub fn best_device<'a>(list: &'a [Device], query: &str) -> Option<&'a Device> {
    let q = query.trim().to_lowercase();
    if let Ok(id) = q.parse::<u32>() {
        return list.iter().find(|d| d.id == id);
    }
    let synonyms: &[&str] = match q.as_str() {
        "headphones" | "headset" | "headphone" => &["headset", "headphone", "hs35", "usb"],
        "speakers" | "speaker" => &["speaker", "analog stereo", "line out"],
        "monitor" | "screen" | "tv" | "display" => &["hdmi", "displayport", "dp"],
        _ => &[],
    };
    list.iter()
        .map(|d| {
            let n = d.name.to_lowercase();
            let mut s = if n.contains(&q) { 100.0 } else { 0.0 };
            for syn in synonyms {
                if n.contains(syn) {
                    s = f64::max(s, 80.0);
                }
            }
            if s == 0.0 {
                s = n.split_whitespace().map(|w| strsim::jaro_winkler(w, &q)).fold(0.0, f64::max) * 60.0;
            }
            (s, d)
        })
        .filter(|(s, _)| *s >= 50.0)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, d)| d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "PipeWire 'pipewire-0' [1.4.7, ciancom@host, cookie:1]
 └─ Clients:
        33. WirePlumber

Audio
 ├─ Devices:
 │      48. CORSAIR HS35 SURROUND v2            [alsa]
 │  
 ├─ Sinks:
 │      47. Family 17h (Models 00h-0fh) HD Audio Controller Digital Stereo (IEC958) [vol: 0.40]
 │  *   57. CORSAIR HS35 SURROUND v2 Analog Stereo [vol: 1.00]
 │      62. GM204 High Definition Audio Controller Digital Stereo (HDMI) [MSI G271] [vol: 0.40 MUTED]
 │  
 ├─ Sources:
 │      46. Family 17h (Models 00h-0fh) HD Audio Controller Analog Stereo [vol: 1.00]
 │  *   58. CORSAIR HS35 SURROUND v2 Mono       [vol: 1.00]
 │  
 └─ Streams:
        82. Chromium

Video
 ├─ Sinks:
 │      99. Not audio [vol: 1.00]
";

    #[test]
    fn parses_wpctl_status() {
        let d = parse_status(STATUS);
        assert_eq!(d.outputs.len(), 3);
        assert_eq!(d.inputs.len(), 2);
        let def = d.outputs.iter().find(|x| x.default).unwrap();
        assert_eq!(def.id, 57);
        assert_eq!(def.volume_percent, Some(100));
        assert_eq!(
            d.outputs[2].name,
            "GM204 High Definition Audio Controller Digital Stereo (HDMI) [MSI G271]"
        );
        assert_eq!(d.outputs[2].volume_percent, Some(40));
        assert!(d.inputs.iter().any(|x| x.default && x.id == 58));
    }

    #[test]
    fn parses_volume_line() {
        assert_eq!(parse_volume("Volume: 0.40"), Some(VolumeState { percent: 40, muted: false }));
        assert_eq!(parse_volume("Volume: 1.00 [MUTED]"), Some(VolumeState { percent: 100, muted: true }));
        assert_eq!(parse_volume("nope"), None);
    }

    #[test]
    fn fuzzy_device_pick() {
        let d = parse_status(STATUS);
        assert_eq!(best_device(&d.outputs, "headphones").unwrap().id, 57);
        assert_eq!(best_device(&d.outputs, "hdmi").unwrap().id, 62);
        assert_eq!(best_device(&d.outputs, "monitor").unwrap().id, 62);
        assert_eq!(best_device(&d.outputs, "corsair").unwrap().id, 57);
        assert_eq!(best_device(&d.outputs, "47").unwrap().id, 47);
        assert!(best_device(&d.outputs, "bluetooth earbuds").is_none());
    }
}
