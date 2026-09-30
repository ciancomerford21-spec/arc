//! End-to-end tests against the real `arcd` binary over its Unix socket.
//!
//! The daemon runs with a temporary socket, config and runtime dir, no
//! language model (`ai.provider = "none"`), and — in the voice test — the
//! real Python voice service in `--no-audio` mode.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Arcd {
    child: Child,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Arcd {
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
    }
}

fn start(extra_config: &str, args: &[&str]) -> Arcd {
    let dir = tempfile::tempdir().unwrap();
    let cfg_dir = dir.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let cfg = cfg_dir.join("config.toml");
    std::fs::write(&cfg, format!("[ai]\nprovider = \"none\"\n{extra_config}")).unwrap();
    let runtime = dir.path().join("run");
    let socket = runtime.join("arc.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_arcd"))
        .arg("--config")
        .arg(&cfg)
        .args(args)
        .env("ARC_SOCKET", &socket)
        .env("ARC_RUNTIME_DIR", &runtime)
        // Deliberately NOT setting ARC_CONFIG_DIR: the voice service must get
        // the config path from the daemon.
        .env("ARC_CONFIG_DIR", dir.path().join("nonexistent"))
        // Never pick up the developer's own ~/.config/arc/automations.toml.
        .env("ARC_AUTOMATIONS", dir.path().join("automations.toml"))
        // Nor their self-made tools or tool classifications: a test that
        // creates or reclassifies a tool must never write the real ones.
        .env("ARC_TOOLS_DIR", dir.path().join("tools"))
        .env("ARC_TOOL_CLASSES", dir.path().join("tool_classes.json"))
        .env("ARC_MEMORY_PATH", dir.path().join("memory"))
        .env("ARC_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn arcd");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "arcd did not create its socket");
        std::thread::sleep(Duration::from_millis(50));
    }
    Arcd { child, socket, _dir: dir }
}

struct Client {
    w: UnixStream,
    r: BufReader<UnixStream>,
    next: u64,
}

impl Client {
    fn connect(p: &Path) -> Self {
        let s = UnixStream::connect(p).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        Self { r: BufReader::new(s.try_clone().unwrap()), w: s, next: 1 }
    }
    fn send(&mut self, mut req: Value) -> u64 {
        let id = self.next;
        self.next += 1;
        req["id"] = json!(id);
        writeln!(self.w, "{req}").unwrap();
        id
    }
    fn line(&mut self) -> Value {
        let mut l = String::new();
        self.r.read_line(&mut l).expect("read");
        assert!(!l.is_empty(), "daemon closed the connection");
        serde_json::from_str(&l).unwrap()
    }
    /// Send a request and return its response, skipping events.
    fn call(&mut self, req: Value) -> Value {
        let id = self.send(req);
        loop {
            let v = self.line();
            if v["kind"] == "response" && v["id"] == id {
                return v;
            }
        }
    }
    fn data(&mut self, req: Value) -> Value {
        let v = self.call(req);
        assert_eq!(v["status"], "ok", "{v}");
        v["data"].clone()
    }
    fn wait_event(&mut self, pred: impl Fn(&Value) -> bool, timeout: Duration) -> Value {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            let v = self.line();
            if v["kind"] == "event" && pred(&v) {
                return v;
            }
        }
        panic!("event not seen");
    }
}

#[test]
fn ping_status_tools() {
    let d = start("", &["--no-voice"]);
    let mut c = Client::connect(&d.socket);
    assert_eq!(c.data(json!({"type": "ping"}))["pong"], true);
    let st = c.data(json!({"type": "status"}));
    assert_eq!(st["state"], "idle");
    assert_eq!(st["ai_provider"], "none");
    assert!(st["tools_total"].as_u64().unwrap() >= 23);
    let tools = c.data(json!({"type": "tools"}));
    let reboot = tools.as_array().unwrap().iter().find(|t| t["name"] == "reboot").unwrap();
    assert_eq!(reboot["risk"], "dangerous");
    let bar = c.data(json!({"type": "bar_status"}));
    assert_eq!(bar["state"], "idle");
}

#[test]
fn socket_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let d = start("", &["--no-voice"]);
    let mode = std::fs::metadata(&d.socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let dir_mode = std::fs::metadata(d.socket.parent().unwrap()).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700);
}

#[test]
fn bad_requests_get_errors_not_disconnects() {
    let d = start("", &["--no-voice"]);
    let mut c = Client::connect(&d.socket);
    writeln!(c.w, "not json").unwrap();
    assert_eq!(c.line()["status"], "error");
    writeln!(c.w, r#"{{"id": 7, "type": "frobnicate"}}"#).unwrap();
    let v = c.line();
    assert_eq!((v["id"].as_u64(), v["status"].as_str()), (Some(7), Some("error")));
    assert_eq!(c.data(json!({"type": "ping"}))["pong"], true);
}

#[test]
fn ask_runs_safe_command_and_holds_dangerous_one() {
    let d = start("", &["--no-voice"]);
    let mut c = Client::connect(&d.socket);
    let r = c.data(json!({"type": "ask", "text": "battery status"}));
    assert_eq!(r["route"], "rule");
    assert_eq!(r["actions"][0]["tool"], "power_info");
    assert_eq!(r["actions"][0]["outcome"], "success");
    let reply = r["reply"].as_str().unwrap();
    assert!(!reply.contains('{') && reply.ends_with('.'), "reply must be speakable: {reply}");

    let r = c.data(json!({"type": "ask", "text": "reboot"}));
    assert_eq!(r["actions"][0]["outcome"], "awaiting_confirmation");
    let id = r["pending"]["confirmation_id"].as_str().unwrap().to_string();
    // Reject it (never actually reboot in a test).
    let r = c.data(json!({"type": "confirm", "confirmation_id": id, "approve": false}));
    assert_eq!(r["reply"], "Cancelled.");
    let r = c.data(json!({"type": "confirm", "confirmation_id": id, "approve": true}));
    assert!(r["reply"].as_str().unwrap().contains("no pending action"), "{r}");
}

#[test]
fn subscribers_receive_events() {
    let d = start("", &["--no-voice"]);
    let mut sub = Client::connect(&d.socket);
    sub.data(json!({"type": "subscribe", "topics": ["assistant"]}));
    let mut c = Client::connect(&d.socket);
    c.data(json!({"type": "ask", "text": "run rm -rf /"}));
    let heard = sub.wait_event(|e| e["event"] == "heard", Duration::from_secs(5));
    assert_eq!(heard["text"], "run rm -rf /");
    let fin = sub.wait_event(|e| e["event"] == "tool_finished", Duration::from_secs(5));
    assert_eq!(fin["record"]["outcome"], "denied");
    sub.wait_event(|e| e["event"] == "reply", Duration::from_secs(5));
}

/// `printf subscribe | socat` pattern: the client half-closes right after
/// subscribing and must keep receiving events.
#[test]
fn subscriber_that_half_closes_keeps_receiving_events() {
    let d = start("", &["--no-voice"]);
    let mut sub = Client::connect(&d.socket);
    sub.data(json!({"type": "subscribe", "topics": ["assistant"]}));
    sub.w.shutdown(std::net::Shutdown::Write).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    Client::connect(&d.socket).data(json!({"type": "ask", "text": "battery status"}));
    let heard = sub.wait_event(|e| e["event"] == "heard", Duration::from_secs(5));
    assert_eq!(heard["text"], "battery status");
}

#[test]
fn second_daemon_refuses_live_socket() {
    let d = start("", &["--no-voice"]);
    let out = Command::new(env!("CARGO_BIN_EXE_arcd"))
        .args(["--no-voice", "--no-bar"])
        .env("ARC_SOCKET", &d.socket)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already listening"));
    // The first daemon still works.
    assert_eq!(Client::connect(&d.socket).data(json!({"type": "ping"}))["pong"], true);
}

/// The whole point of the feature, end to end: the playback script's report
/// comes back out of the real socket as an event the overlay subscribes to,
/// and the row clears when the player process is gone.
///
/// The report is made over the socket rather than by running the script,
/// because the script's `arc` call is the same call -- running mpv and yt-dlp
/// in a test would need both, a network, and would actually play music.
#[test]
fn now_playing_reaches_subscribers_and_clears_when_the_player_dies() {
    let d = start("", &["--no-voice"]);
    let mut sub = Client::connect(&d.socket);
    sub.data(json!({"type": "subscribe", "topics": ["assistant"]}));
    let mut c = Client::connect(&d.socket);

    // Nothing playing at first.
    let empty = c.data(json!({"type": "now_playing", "op": "show"}));
    assert_eq!(empty["state"], "stopped");
    assert_eq!(empty["playing"], false);
    assert_eq!(c.data(json!({"type": "bar_status"})).get("now_playing"), None);

    // A report from a playback tool.
    let set = c.data(json!({
        "type": "now_playing", "op": "set",
        "title": "Hall of Fame", "artist": "Boards of Canada",
        "source": "youtube music", "pid": 0,
    }));
    assert_eq!(set["playing"], true);
    assert_eq!(set["label"], "Hall of Fame — Boards of Canada");

    let ev = sub.wait_event(|e| e["event"] == "now_playing", Duration::from_secs(5));
    assert_eq!(ev["status"]["title"], "Hall of Fame");
    assert_eq!(ev["status"]["artist"], "Boards of Canada");
    assert_eq!(ev["status"]["label"], "Hall of Fame — Boards of Canada");
    // The overlay's own read agrees with what it was told.
    assert_eq!(c.data(json!({"type": "now_playing", "op": "show"}))["title"], "Hall of Fame");
    let bar = c.data(json!({"type": "bar_status"}));
    assert_eq!(bar["now_playing"]["title"], "Hall of Fame");

    // Stopping clears it and says so.
    let stopped = c.data(json!({"type": "now_playing", "op": "stop"}));
    assert_eq!(stopped["playing"], false);
    let ev = sub.wait_event(
        |e| e["event"] == "now_playing" && e["status"]["playing"] == false,
        Duration::from_secs(5),
    );
    assert_eq!(ev["status"]["title"], "", "a stop must not leave the old title attached: {ev}");
    assert_eq!(c.data(json!({"type": "bar_status"})).get("now_playing"), None);
}

/// A blank title is a client bug and must be an error, not an empty row on the
/// overlay that nobody can tell apart from a rendering fault.
#[test]
fn a_now_playing_report_without_a_title_is_refused() {
    let d = start("", &["--no-voice"]);
    let mut c = Client::connect(&d.socket);
    let v = c.call(json!({"type": "now_playing", "op": "set", "artist": "Nobody"}));
    assert_eq!(v["status"], "error");
    assert_eq!(v["error"]["code"], "bad_request");
    assert!(c.data(json!({"type": "bar_status"})).get("now_playing").is_none());
}

/// The reap is the only thing that clears the row when a track ends on its own:
/// the tool that started it has already exited by then. Report a pid that cannot
/// exist and the row must clear on its own, within a second or two, with no
/// further requests.
#[test]
fn a_dead_player_clears_the_row_without_being_asked() {
    let d = start("", &["--no-voice"]);
    let mut sub = Client::connect(&d.socket);
    sub.data(json!({"type": "subscribe", "topics": ["assistant"]}));
    let mut c = Client::connect(&d.socket);
    // u32::MAX is above pid_t's range, so it can never be a real process.
    c.data(json!({"type": "now_playing", "op": "set", "title": "Already Gone", "pid": u32::MAX}));
    let ev = sub.wait_event(
        |e| e["event"] == "now_playing" && e["status"]["playing"] == false,
        Duration::from_secs(10),
    );
    assert_eq!(ev["status"]["title"], "", "{ev}");
    assert_eq!(c.data(json!({"type": "now_playing", "op": "show"}))["playing"], false);
}

fn voice_available() -> bool {
    let py = arc_config_python();
    py.exists() && Path::new(env!("CARGO_MANIFEST_DIR")).join("../../python/arc_voice").exists()
}

fn arc_config_python() -> PathBuf {
    std::env::var_os("ARC_VOICE_PYTHON").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").unwrap()).join(".local/share/arc/venv/bin/python")
    })
}

/// Full voice round trip through the real sidecar (no audio devices):
/// daemon starts it, it reports health, a spoken reply is synthesised and
/// its lifecycle comes back through the daemon.
#[test]
fn voice_service_round_trip() {
    if !voice_available() {
        eprintln!("skipping: voice venv not installed");
        return;
    }
    let d = start("[voice]\nspeak_text_replies = true\nmode = \"push_to_talk\"\n", &["--voice-no-audio"]);
    let mut c = Client::connect(&d.socket);
    // Wait for the sidecar to report health (models load in ~2 s).
    let end = Instant::now() + Duration::from_secs(30);
    let status = loop {
        let st = c.data(json!({"type": "status"}));
        if st["voice_mode"].is_string() {
            break st;
        }
        assert!(Instant::now() < end, "voice service never reported health: {st}");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(status["voice_mode"], "push_to_talk");
    let comp =
        |n: &str| status["components"].as_array().unwrap().iter().find(|c| c["name"] == n).cloned().unwrap();
    assert_eq!(comp("voice.stt")["status"], "ok");
    assert_eq!(comp("voice.mic")["status"], "disabled");

    c.data(json!({"type": "subscribe", "topics": ["assistant"]}));
    c.data(json!({"type": "ask", "text": "battery status"}));
    // speak_text_replies = true -> the reply is spoken: Speaking then Idle.
    c.wait_event(|e| e["event"] == "state" && e["state"] == "speaking", Duration::from_secs(20));
    c.wait_event(|e| e["event"] == "state" && e["state"] == "idle", Duration::from_secs(20));

    // Voice control commands are accepted once the service is up.
    assert_eq!(c.data(json!({"type": "voice", "command": {"action": "start_listening"}}))["sent"], true);
    c.wait_event(|e| e["event"] == "state" && e["state"] == "listening", Duration::from_secs(10));
    c.data(json!({"type": "voice", "command": {"action": "cancel_listening"}}));
    c.wait_event(|e| e["event"] == "state" && e["state"] == "idle", Duration::from_secs(10));
}
