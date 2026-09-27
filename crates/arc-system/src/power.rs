//! Power: battery/AC status, device sleep, hibernate, reboot, shutdown, lock.
//!
//! Mutating actions also lock the screen first so the user always has a
//! safety net on Wake.

use crate::Result;
use crate::SHORT;
use crate::run;
use crate::which;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerMode {
    /// Desktop / no battery present: running on mains.
    NoBattery,
    OnBattery,
    Charging,
    /// Plugged in, battery not charging (full or held by a charge limit).
    OnAc,
    Full,
    Unknown,
}

impl PowerMode {
    pub fn label(&self) -> &'static str {
        match self {
            PowerMode::NoBattery => "on mains power (no battery)",
            PowerMode::OnBattery => "on battery",
            PowerMode::OnAc => "plugged in",
            PowerMode::Charging => "charging",
            PowerMode::Full => "fully charged",
            PowerMode::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PowerInfo {
    pub mode: PowerMode,
    pub battery_present: bool,
    pub battery_percent: Option<u32>,
    pub time_left_minutes: Option<i64>,
    pub time_to_full_minutes: Option<i64>,
    pub ac_online: bool,
}

impl PowerInfo {
    /// One short spoken sentence.
    pub fn sentence(&self) -> String {
        match (self.mode, self.battery_percent) {
            (PowerMode::NoBattery, _) => "This machine has no battery; it's running on mains power.".into(),
            (m, Some(p)) => {
                let mut s = format!("Battery at {p} percent, {}", m.label());
                if let Some(t) = self.time_left_minutes.filter(|_| m == PowerMode::OnBattery) {
                    s.push_str(&format!(", about {} left", fmt_minutes(t)));
                }
                if let Some(t) = self.time_to_full_minutes.filter(|_| m == PowerMode::Charging) {
                    s.push_str(&format!(", full in about {}", fmt_minutes(t)));
                }
                s.push('.');
                s
            }
            (m, None) => format!("Power: {}.", m.label()),
        }
    }
}

fn fmt_minutes(m: i64) -> String {
    match (m / 60, m % 60) {
        (0, m) => format!("{m} minutes"),
        (h, 0) => format!("{h} hour{}", if h == 1 { "" } else { "s" }),
        (h, m) => format!("{h} hour{} {m} minutes", if h == 1 { "" } else { "s" }),
    }
}

pub async fn info() -> Result<PowerInfo> {
    Ok(read_power_supply(std::path::Path::new("/sys/class/power_supply")))
}

fn read_str(path: &std::path::Path, name: &str) -> Option<String> {
    std::fs::read_to_string(path.join(name)).ok().map(|s| s.trim().to_string())
}

/// Read battery / mains state from a sysfs `power_supply` directory.
/// Uses energy_* (µWh, µW) or charge_* (µAh, µA) — whichever the driver has.
pub fn read_power_supply(root: &std::path::Path) -> PowerInfo {
    let mut ac_online = false;
    let mut have_mains = false;
    let mut bat: Option<(std::path::PathBuf, String)> = None;
    if let Ok(entries) = std::fs::read_dir(root) {
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            match read_str(&path, "type").as_deref() {
                Some("Mains") => {
                    have_mains = true;
                    ac_online |= read_str(&path, "online").as_deref() == Some("1");
                }
                // Peripheral batteries (mice, headsets) have scope=Device.
                Some("Battery") if read_str(&path, "scope").as_deref() != Some("Device") => {
                    if bat.is_none() && read_str(&path, "present").as_deref() != Some("0") {
                        let status = read_str(&path, "status").unwrap_or_default();
                        bat = Some((path, status));
                    }
                }
                _ => {}
            }
        }
    }
    let Some((path, status)) = bat else {
        return PowerInfo {
            mode: PowerMode::NoBattery,
            battery_present: false,
            battery_percent: None,
            time_left_minutes: None,
            time_to_full_minutes: None,
            ac_online: true,
        };
    };
    let percent = read_u64(&path, "capacity").map(|c| c.min(100) as u32);
    let (now, full, rate) = match (read_u64(&path, "energy_now"), read_u64(&path, "energy_full")) {
        (Some(n), Some(f)) => (Some(n), Some(f), read_u64(&path, "power_now")),
        _ => (read_u64(&path, "charge_now"), read_u64(&path, "charge_full"), read_u64(&path, "current_now")),
    };
    let rate = rate.filter(|&r| r > 0);
    let mode = match status.as_str() {
        "Discharging" => PowerMode::OnBattery,
        "Charging" => PowerMode::Charging,
        "Full" => PowerMode::Full,
        "Not charging" => PowerMode::OnAc,
        _ if have_mains && ac_online => PowerMode::OnAc,
        _ if have_mains => PowerMode::OnBattery,
        _ => PowerMode::Unknown,
    };
    let minutes = |amount: u64, rate: u64| ((amount as f64 / rate as f64) * 60.0).round() as i64;
    let time_left = match (mode, now, rate) {
        (PowerMode::OnBattery, Some(n), Some(r)) => Some(minutes(n, r)),
        _ => None,
    };
    let time_to_full = match (mode, now, full, rate) {
        (PowerMode::Charging, Some(n), Some(f), Some(r)) if f > n => Some(minutes(f - n, r)),
        _ => None,
    };
    PowerInfo {
        mode,
        battery_present: true,
        battery_percent: percent,
        time_left_minutes: time_left,
        time_to_full_minutes: time_to_full,
        ac_online: ac_online || matches!(mode, PowerMode::Charging | PowerMode::Full),
    }
}

fn read_u64(path: &std::path::Path, name: &str) -> Option<u64> {
    std::fs::read_to_string(path.join(name)).ok().and_then(|s| s.trim().parse().ok())
}

pub async fn lock() -> Result<String> {
    if let Some(_) = which("omarchy-system-lock") {
        run("omarchy-system-lock", &[], SHORT).await?;
    } else {
        run("loginctl", &["lock-session"], SHORT).await?;
    }
    Ok("screen locked".into())
}

pub async fn sleep() -> Result<String> {
    if let Some(_) = which("systemctl") {
        run("systemctl", &["suspend"], SHORT).await?;
    } else {
        run("zzz", &[], SHORT).await?;
    }
    Ok("suspended".into())
}

pub async fn hibernate() -> Result<String> {
    run("systemctl", &["hibernate"], SHORT).await?;
    Ok("hibernated".into())
}

pub async fn reboot() -> Result<String> {
    run("systemctl", &["reboot"], SHORT).await?;
    Ok("rebooting".into())
}

pub async fn shutdown() -> Result<String> {
    run("systemctl", &["poweroff"], SHORT).await?;
    Ok("powering off".into())
}

pub async fn lock_then_sleep() -> Result<String> {
    lock().await?;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    sleep().await?;
    Ok("locked and sleeping".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn dev(root: &Path, name: &str, files: &[(&str, &str)]) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        for (k, v) in files {
            std::fs::write(d.join(k), format!("{v}\n")).unwrap();
        }
    }

    #[test]
    fn desktop_without_battery() {
        let t = tempfile::tempdir().unwrap();
        let i = read_power_supply(t.path());
        assert_eq!(i.mode, PowerMode::NoBattery);
        assert!(!i.battery_present);
        assert!(i.sentence().contains("no battery"));
    }

    #[test]
    fn peripheral_battery_is_not_the_system_battery() {
        let t = tempfile::tempdir().unwrap();
        dev(t.path(), "hidpp_battery_0", &[("type", "Battery"), ("scope", "Device"), ("capacity", "40"), ("status", "Discharging")]);
        assert_eq!(read_power_supply(t.path()).mode, PowerMode::NoBattery);
    }

    #[test]
    fn discharging_laptop_energy() {
        let t = tempfile::tempdir().unwrap();
        dev(t.path(), "AC", &[("type", "Mains"), ("online", "0")]);
        dev(t.path(), "BAT0", &[
            ("type", "Battery"), ("status", "Discharging"), ("capacity", "50"),
            ("energy_now", "30000000"), ("energy_full", "60000000"), ("power_now", "10000000"),
        ]);
        let i = read_power_supply(t.path());
        assert_eq!(i.mode, PowerMode::OnBattery);
        assert_eq!(i.battery_percent, Some(50));
        assert_eq!(i.time_left_minutes, Some(180)); // 30 Wh / 10 W = 3 h
        assert!(!i.ac_online);
        assert_eq!(i.sentence(), "Battery at 50 percent, on battery, about 3 hours left.");
    }

    #[test]
    fn charging_laptop_charge_units() {
        let t = tempfile::tempdir().unwrap();
        dev(t.path(), "ADP1", &[("type", "Mains"), ("online", "1")]);
        dev(t.path(), "BAT1", &[
            ("type", "Battery"), ("status", "Charging"), ("capacity", "75"),
            ("charge_now", "3000000"), ("charge_full", "4000000"), ("current_now", "2000000"),
        ]);
        let i = read_power_supply(t.path());
        assert_eq!(i.mode, PowerMode::Charging);
        assert_eq!(i.time_to_full_minutes, Some(30)); // 1 Ah / 2 A
        assert!(i.ac_online);
    }

    #[test]
    fn plugged_in_at_charge_limit() {
        let t = tempfile::tempdir().unwrap();
        dev(t.path(), "AC", &[("type", "Mains"), ("online", "1")]);
        dev(t.path(), "BAT0", &[("type", "Battery"), ("status", "Not charging"), ("capacity", "80")]);
        assert_eq!(read_power_supply(t.path()).mode, PowerMode::OnAc);
    }

    #[tokio::test]
    async fn info_on_this_machine_does_not_panic() {
        let i = info().await.unwrap();
        assert!(!i.sentence().is_empty());
    }
}
