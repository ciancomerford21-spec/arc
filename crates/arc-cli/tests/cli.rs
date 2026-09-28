//! `arc` CLI against a real `arcd` (no AI, no voice) on a temp socket.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn target_dir() -> PathBuf {
    // tests/../../../target/debug
    PathBuf::from(env!("CARGO_BIN_EXE_arc")).parent().unwrap().to_path_buf()
}

struct Arcd {
    child: Child,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Arcd {
    fn drop(&mut self) {
        let _ = Command::new("kill").arg(self.child.id().to_string()).status();
        let _ = self.child.wait();
    }
}

fn arcd() -> Arcd {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "[ai]\nprovider = \"none\"\n").unwrap();
    let socket = dir.path().join("run/arc.sock");
    let bin = target_dir().join("arcd");
    assert!(bin.exists(), "build arcd first: cargo build -p arc-daemon");
    let child = Command::new(bin)
        .args(["--no-voice", "--no-bar", "--config"])
        .arg(&cfg)
        .env("ARC_SOCKET", &socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(20);
    while !socket.exists() {
        assert!(Instant::now() < end, "arcd did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    Arcd { child, socket, _dir: dir }
}

fn arc(sock: &Path, args: &[&str]) -> (bool, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_arc")).args(args).env("ARC_SOCKET", sock).output().unwrap();
    (
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

#[test]
fn ask_and_status() {
    let d = arcd();
    let (ok, out, _) = arc(&d.socket, &["ask", "battery", "status"]);
    assert!(ok);
    assert!(out.trim().ends_with('.'), "{out}");
    let (ok, out, _) = arc(&d.socket, &["status"]);
    assert!(ok && out.contains("state idle") && out.contains("daemon"), "{out}");
    let (ok, out, _) = arc(&d.socket, &["--json", "status"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["state"], "idle");
}

#[test]
fn reboot_is_held_and_reject_without_id_cancels_it() {
    let d = arcd();
    let (ok, out, err) = arc(&d.socket, &["ask", "reboot"]);
    assert!(ok);
    assert!(out.contains("Confirm?"), "{out}");
    assert!(err.contains("arc confirm "), "{err}");
    let (ok, out, _) = arc(&d.socket, &["reject"]);
    assert!(ok);
    assert_eq!(out.trim(), "Cancelled.");
}

#[test]
fn tools_list_shows_risk() {
    let d = arcd();
    let (ok, out, _) = arc(&d.socket, &["tools"]);
    assert!(ok);
    let reboot = out.lines().find(|l| l.starts_with("reboot")).unwrap();
    assert!(reboot.contains("dangerous"), "{reboot}");
}

#[test]
fn bar_output_formats() {
    let d = arcd();
    let (ok, out, _) = arc(&d.socket, &["bar", "--waybar"]);
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["alt"], "idle");
    assert!(v["class"].is_array());
    assert!(v["tooltip"].as_str().unwrap().contains("Arc"));
}

#[test]
fn bar_when_daemon_is_down_is_offline_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (ok, out, _) = arc(&dir.path().join("nope.sock"), &["bar", "--waybar"]);
    assert!(ok, "bars must never see a failing command");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["alt"], "offline");
}

#[test]
fn bar_follow_emits_on_state_change() {
    use std::io::{BufRead, BufReader};
    let d = arcd();
    let mut f = Command::new(env!("CARGO_BIN_EXE_arc"))
        .args(["bar", "--follow"])
        .env("ARC_SOCKET", &d.socket)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(f.stdout.take().unwrap()).lines();
    let first: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(first["state"], "idle");
    arc(&d.socket, &["ask", "battery status"]);
    // thinking -> idle
    let states: Vec<String> = (0..2)
        .map(|_| {
            serde_json::from_str::<serde_json::Value>(&lines.next().unwrap().unwrap()).unwrap()["state"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(states, ["thinking", "idle"]);
    let _ = f.kill();
    let _ = f.wait();
}

#[test]
fn unreachable_daemon_has_a_helpful_error() {
    let dir = tempfile::tempdir().unwrap();
    let (ok, _, err) = arc(&dir.path().join("nope.sock"), &["status"]);
    assert!(!ok);
    assert!(err.contains("is arcd running"), "{err}");
}
