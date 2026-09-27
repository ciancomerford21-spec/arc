//! Network status: interface state, reachability, public IP.
//!
//! Degrades gracefully: every helper is optional, so a headless build still
//! compiles and every public function returns something useful.

use crate::Result;
use crate::SysError;
use serde::Serialize;
use std::net::IpAddr;

/// Single network interface summary.
#[derive(Debug, Clone, Serialize)]
pub struct Interface {
    pub name: String,
    pub kind: String,
    pub state: String,
    pub connection: Option<String>,
    pub gateway: Option<String>,
    pub address: Option<String>,
    pub reachable: bool,
    pub public_ip: Option<IpAddr>,
}

/// Primary interface (first non-loopback, or the default route target).
pub async fn primary() -> Result<Interface> {
    let mut iface = Interface {
        name: String::new(),
        kind: String::new(),
        state: "unknown".into(),
        connection: None,
        gateway: None,
        address: None,
        reachable: false,
        public_ip: None,
    };
    if let Some(nc) = nmcli_status().await.ok().flatten() {
        iface = nc;
    }
    iface.gateway = gateway_default();
    iface.reachable = ping();
    iface.public_ip = public_ip_sync();
    if iface.name.is_empty() {
        if let Some(def) = gateway_default() {
            iface.name = def;
            iface.kind = "unknown".into();
            iface.state = "no-interface".into();
        }
    }
    Ok(iface)
}

async fn nmcli_status() -> Result<Option<Interface>> {
    let out = crate::run(
        "nmcli",
        &["-t", "-f", "DEVICE,TYPE,STATE,CONNECTION,IP4.ADDRESS", "dev"],
        crate::SHORT,
    ).await?;
    for line in out.lines() {
        let mut parts = line.split(':');
        let dev = match parts.next() {
            Some(s) => s.trim().to_string(),
            None => return Ok(None),
        };
        if dev == "lo" {
            continue;
        }
        let kind = match parts.next() {
            Some(s) => s.trim().to_string(),
            None => return Ok(None),
        };
        let state = match parts.next() {
            Some(s) => s.trim().to_string(),
            None => return Ok(None),
        };
        let conn = match parts.next() {
            Some(s) => s.trim().to_string(),
            None => return Ok(None),
        };
        let addr = match parts.next() {
            Some(s) => s.trim().to_string(),
            None => return Ok(None),
        };
        return Ok(Some(Interface {
            name: dev,
            kind,
            state,
            connection: if conn.is_empty() { None } else { Some(conn) },
            gateway: None,
            address: if addr.is_empty() { None } else { Some(addr) },
            reachable: false,
            public_ip: None,
        }));
    }
    Ok(None)
}

fn gateway_default() -> Option<String> {
    std::process::Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .ok()
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .and_then(|l| {
                    let mut w = l.split_whitespace();
                    if w.next()? == "default" {
                        w.next()?.strip_prefix("via ").map(|g| g.to_string())
                    } else {
                        None
                    }
                })
        })
}

fn ping() -> bool {
    std::process::Command::new("ping")
        .args(["-c", "1", "-W", "2", "1.1.1.1"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn public_ip_sync() -> Option<IpAddr> {
    #[cfg(feature = "network-sync")]
    {
        match reqwest::blocking::get("https://api.ipify.org") {
            Ok(resp) => resp.text().ok().and_then(|s| s.parse().ok()),
            Err(_) => None,
        }
    }
    #[cfg(not(feature = "network-sync"))]
    {
        None
    }
}

/// Async variant that queries public IP over the network.
pub async fn public_ip() -> Result<Option<IpAddr>> {
    let resp = reqwest::get("https://api.ipify.org").await
        .map_err(|e| SysError::DBus(format!("reqwest error: {e}")))?;
    let s = resp.text().await
        .map_err(|e| SysError::DBus(format!("reqwest error: {e}")))?;
    Ok(s.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nmcli_parse_on_device_fields() {
        let inp = "enp4s0:ethernet:connected:'Wired connection 1':192.168.1.10\nlo:loopback:unmanaged:'':\n";
        let parsed = nmcli_parse(inp);
        assert_eq!(parsed.name, "enp4s0");
        assert_eq!(parsed.kind, "ethernet");
        assert_eq!(parsed.state, "connected");
    }

    fn nmcli_parse(line: &str) -> Interface {
        let mut parts = line.split(':');
        let dev = parts.next().unwrap().trim().to_string();
        let kind = parts.next().unwrap().trim().to_string();
        let state = parts.next().unwrap().trim().to_string();
        let conn = parts.next().unwrap().trim().to_string();
        let addr = parts.next().unwrap().trim().to_string();
        Interface {
            name: dev,
            kind,
            state,
            connection: Some(if conn.is_empty() || (conn.starts_with('\'') && conn.len() > 2) {
                conn[1..conn.len() - 1].to_string()
            } else {
                conn
            }),
            gateway: None,
            address: if addr.is_empty() || (addr.starts_with('\'') && addr.len() > 2) {
                None
            } else {
                Some(addr)
            },
            reachable: false,
            public_ip: None,
        }
    }

    #[test]
    fn parse_ipv4_ok() {
        assert_eq!("192.168.1.1".parse::<IpAddr>().ok(), Some(IpAddr::from([192u8, 168, 1, 1])));
    }
}
