//! Lightweight system/resource monitor. Reads from sysinfo plus a few
//! helper commands so Arc can answer "what's slowing me down?" and
//! "what's using my GPU?" without any LLM.
//!
//! **Capabilities (read only):**
//! * Quick overview: cpus, memory, swap, disks, load.
//! * Per-process RAM/CPU ranking.
//! * NVIDIA GPU utilisation, VRAM usage, temperature (when present).
//! * Logged-in users and uptime.
//!
//! Degrades gracefully: if a helper is missing the corresponding field
//! is simply `None`.

use crate::Result;
use crate::which;
use serde::Serialize;
use std::time::Duration;
use sysinfo::{Disks, System, Cpu};

#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub uptime_seconds: u64,
    pub load_avg: Option<[f64; 3]>,
    pub memory: Memory,
    pub swap: Option<Swap>,
    pub cpu_percent: f64,
    pub cpu_cores: u32,
    pub gpu: Option<Gpu>,
    pub disks: Vec<Disk>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Memory {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub used_percent: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Swap {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub used_percent: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Gpu {
    pub name: String,
    pub utilization_percent: Option<f64>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    pub temperature_celsius: Option<f64>,
    pub power_watts: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Disk {
    pub mount: String,
    pub fs_type: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_bytes: u64,
    pub used_percent: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub command: String,
    pub memory_bytes: u64,
    pub cpu_percent: f64,
}

#[allow(dead_code)]
pub async fn overview() -> Result<Overview> {
    let mut s = System::new_all();
    s.refresh_all();

    std::thread::sleep(Duration::from_millis(25));
    let mut s2 = System::new_all();
    s2.refresh_all();
    s2.refresh_cpu_usage();

    let cpu_cores = s.cpus().len() as u32;
    let cpu_pct: f64 = if cpu_cores > 0 {
        let before: f64 = s.cpus().iter().map(|c: &Cpu| c.cpu_usage() as f64).sum();
        let after: f64 = s2.cpus().iter().map(|c: &Cpu| c.cpu_usage() as f64).sum();
        let delta = after - before;
        if delta >= 0.0 { delta / cpu_cores as f64 } else { 0.0 }
    } else {
        0.0
    };
    let cpu_percent = (cpu_pct * 100.0).clamp(0.0, 100.0);

    let total_mem = s.total_memory();
    let used_mem = s.used_memory();
    let used_pct = if total_mem > 0 { 100.0 * (used_mem as f64) / (total_mem as f64) } else { 0.0 };

    let total_swap = s.total_swap();
    let used_swap = s.used_swap();
    let swap = if total_swap > 0 {
        Some(Swap {
            total_bytes: total_swap,
            used_bytes: used_swap,
            used_percent: 100.0 * (used_swap as f64) / (total_swap as f64),
        })
    } else {
        None
    };

    Ok(Overview {
        uptime_seconds: System::uptime(),
        load_avg: load_avg(),
        memory: Memory {
            total_bytes: total_mem,
            used_bytes: used_mem,
            available_bytes: s.available_memory(),
            used_percent: used_pct,
        },
        swap,
        cpu_percent,
        cpu_cores,
        gpu: gpu_nvidia(),
        disks: disk_list(),
    })
}

fn load_avg() -> Option<[f64; 3]> {
    std::fs::read_to_string("/proc/loadavg").ok().and_then(|v| {
        let mut parts = v.split_whitespace();
        let a = parts.next()?.parse::<f64>().ok()?;
        let b = parts.next()?.parse::<f64>().ok()?;
        let c = parts.next()?.parse::<f64>().ok()?;
        Some([a, b, c])
    })
}

/// NVIDIA GPU stats via `nvidia-smi --format=json` (optional).
pub fn gpu_nvidia() -> Option<Gpu> {
    let bin = which("nvidia-smi")?;
    let out = std::process::Command::new(bin)
        .args(["--format=json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str::<NvidiaSmiJson>(&text).ok().and_then(|j| {
        let gpu = j.gpu.first()?;
        let util = gpu.get("utilization")
            .and_then(|u| u.get("utilization.gpu [%]"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok());
        let mem_used = gpu.get("fb_memory_usage")
            .and_then(|m| m.get("used [MiB]"))
            .and_then(|v| v.as_str())
            .and_then(|s| (s.parse::<f64>().ok()).map(|m| (m * 1024.0 * 1024.0) as u64));
        let mem_total = gpu.get("fb_memory_usage")
            .and_then(|m| m.get("total [MiB]"))
            .and_then(|v| v.as_str())
            .and_then(|s| (s.parse::<f64>().ok()).map(|m| (m * 1024.0 * 1024.0) as u64));
        let temp = gpu.get("temperature")
            .and_then(|t| t.get("gpu_temp"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok());
        let power = gpu.get("power_readings")
            .and_then(|p| p.get("power_draw [W]"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok());
        Some(Gpu {
            name: gpu.get("product_name")
                .and_then(|v| v.as_str())
                .unwrap_or("NVIDIA GPU")
                .to_string(),
            utilization_percent: util,
            memory_used_bytes: mem_used,
            memory_total_bytes: mem_total,
            temperature_celsius: temp,
            power_watts: power,
        })
    })
}

#[derive(serde::Deserialize)]
struct NvidiaSmiJson {
    gpu: Vec<serde_json::Value>,
}

fn disk_list() -> Vec<Disk> {
    let disks = Disks::new_with_refreshed_list();
    disks.iter().map(|d| Disk {
        mount: d.name().to_string_lossy().to_string(),
        fs_type: d.file_system().to_string_lossy().to_string(),
        total_bytes: d.total_space(),
        available_bytes: d.available_space(),
        used_bytes: d.total_space().saturating_sub(d.available_space()),
        used_percent: if d.total_space() > 0 {
            100.0 * (d.total_space() - d.available_space()) as f64 / d.total_space() as f64
        } else {
            0.0
        },
    }).collect()
}

/// Top RAM consumers, sorted by memory.
#[allow(dead_code)]
pub async fn top_memory(top: usize) -> Result<Vec<ProcessInfo>> {
    let mut sys = System::new_all();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let mut items: Vec<ProcessInfo> = sys
        .processes()
        .iter()
        .map(|(_pid, p)| {
            let cpu_pct = p.cpu_usage() as f64;
            ProcessInfo {
                pid: p.pid().as_u32(),
                name: p.name().to_string_lossy().to_string(),
                command: p.cmd().iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>().join(" "),
                memory_bytes: p.memory(),
                cpu_percent: cpu_pct,
            }
        })
        .collect();

    items.sort_by(|a, b| b.memory_bytes.cmp(&a.memory_bytes));
    Ok(items.into_iter().take(top).collect())
}

/// Top CPU consumers, sorted by CPU usage.
#[allow(dead_code)]
pub async fn top_cpu(top: usize) -> Result<Vec<ProcessInfo>> {
    let mut sys = System::new_all();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let mut items: Vec<ProcessInfo> = sys
        .processes()
        .iter()
        .map(|(_pid, p)| {
            let cpu_pct = p.cpu_usage() as f64;
            ProcessInfo {
                pid: p.pid().as_u32(),
                name: p.name().to_string_lossy().to_string(),
                command: p.cmd().iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>().join(" "),
                memory_bytes: p.memory(),
                cpu_percent: cpu_pct,
            }
        })
        .collect();

    items.sort_by(|a, b| b.cpu_percent.partial_cmp(&a.cpu_percent)
        .unwrap_or(std::cmp::Ordering::Equal));
    Ok(items.into_iter().take(top).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_avg_from_proc() {
        let avg = load_avg();
        assert!(avg.is_some());
        let [a, b, c] = avg.unwrap();
        assert!(a >= 0.0 && a <= 1000.0);
        assert!(b >= 0.0 && b <= 1000.0);
        assert!(c >= 0.0 && c <= 1000.0);
    }

    #[tokio::test]
    async fn overview_is_reasonable() {
        let o = overview().await.unwrap();
        assert!(o.memory.total_bytes > 1_000_000_000);
        assert!(o.disks.len() >= 1);
        assert!(o.cpu_percent >= 0.0 && o.cpu_percent <= 100.0);
    }

    #[tokio::test]
    async fn top_memory_returns_some() {
        let t = top_memory(5).await.unwrap();
        assert!(t.len() <= 5);
    }

    #[tokio::test]
    async fn top_cpu_returns_some() {
        let t = top_cpu(5).await.unwrap();
        assert!(t.len() <= 5);
    }
}
