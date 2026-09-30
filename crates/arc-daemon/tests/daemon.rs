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

// ---------------------------------------------------------------------------
// Music
// ---------------------------------------------------------------------------

/// A stand-in for mpv that speaks mpv's JSON IPC protocol, and a stand-in for
/// yt-dlp.
///
/// The point is that nothing above the player is faked: the daemon starts this
/// as a real child process, connects to a real Unix socket, and speaks the real
/// protocol to it. So these tests cover the wire format too -- if a command
/// name or a property name is wrong, the stub answers `invalid parameter`
/// exactly as mpv does, and the test fails. Starting a real mpv would play
/// audio on whoever is running the suite.
fn write_player_stub(dir: &Path, log: &Path) -> PathBuf {
    let script = dir.join("fake-mpv");
    std::fs::write(
        &script,
        format!(
            r#"#!/usr/bin/env python3
import json, os, socket, sys
# Only the `=` form. Given the space form this mpv reads the path as a file
# to play rather than as the option's value, so accepting it here would let the
# suite pass on a spawn that can never work on the real binary.
sock = next(a.split("=", 1)[1] for a in sys.argv if a.startswith("--input-ipc-server="))
log = open({log:?}, "a", buffering=1)
if os.path.exists(sock):
    os.remove(sock)
srv = socket.socket(socket.AF_UNIX)
srv.bind(sock)
srv.listen(1)
conn, _ = srv.accept()
buf = b""
paused = False
while True:
    data = conn.recv(65536)
    if not data:
        break
    buf += data
    while b"\n" in buf:
        line, buf = buf.split(b"\n", 1)
        if not line.strip():
            continue
        req = json.loads(line)
        cmd = req.get("command", [])
        log.write(json.dumps(cmd) + "\n")
        rid = req.get("request_id")
        name = cmd[0] if cmd else ""
        if name == "get_property":
            prop = cmd[1]
            data = {{"playlist-pos": 0, "playlist-count": 2, "pause": paused,
                     "time-pos": 7.0, "duration": 233.0, "idle-active": False}}.get(prop)
        elif name == "set_property" and cmd[1] == "pause":
            paused = bool(cmd[2])
            data = None
        else:
            data = None
        reply = {{"data": data, "request_id": rid, "error": "success"}}
        if name == "frobnicate":
            reply["error"] = "invalid parameter"
        conn.sendall((json.dumps(reply) + "\n").encode())
"#,
            log = log.display().to_string()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    script
}

/// A stand-in for yt-dlp: one line of `--print` output per result.
fn write_resolver_stub(dir: &Path) -> PathBuf {
    let script = dir.join("fake-yt-dlp");
    std::fs::write(
        &script,
        "#!/usr/bin/env python3\nimport sys\n# Everything after \"ytsearchN:\" is the query the caller searched for.\nargs = sys.argv[1:]\nquery = \"\"\nfor a in args:\n    if a.startswith(\"ytsearch\"):\n        query = a.split(\":\", 1)[1]\nn = 1\nfor a in args:\n    if a.startswith(\"ytsearch\"):\n        n = int(a[len(\"ytsearch\"):].split(\":\")[0] or 1)\nfor i in range(n):\n    print(\"%s %d|||Boards of Canada|||https://example.invalid/stream%d\" % (query, i, i))\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    script
}

/// Start a daemon wired to the stubs, and hand back the command log path.
fn start_with_music() -> (Arcd, PathBuf) {
    // A directory that outlives this function: the daemon reads the stubs out
    // of it for the whole run, and a TempDir would delete them mid-test.
    // Unique per call, because these tests run in parallel in one binary and
    // two daemons sharing one player stub would answer each other's commands.
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("arc-music-stub-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("player.log");
    let player = write_player_stub(&dir, &log);
    let resolver = write_resolver_stub(&dir);
    let cfg = format!(
        // `search` pinned to yt-dlp: this fixture is about the player and the
        // queue, and leaving it on the default would make every one of these
        // tests pay for a failed `python -m arc_music` and a fallback first.
        // The API path has its own test below.
        "[music]\nenabled = true\nbrowser_fallback = false\nsearch = \"yt-dlp\"\nplayer = {:?}\nresolver = {:?}\n",
        player.display().to_string(),
        resolver.display().to_string()
    );
    let d = start(&cfg, &["--no-voice"]);
    // Hold the TempDir open for as long as the daemon: the stubs are read from
    // it for the whole run, and dropping it would delete them mid-test.
    (d, log)
}

fn player_log(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log).unwrap_or_default().lines().map(str::to_string).collect()
}

/// The whole feature, end to end: a search resolves, the player is told to
/// play it, a subscriber sees it, the transport reaches the player, and
/// stopping clears everything.
#[test]
fn music_plays_pauses_queues_and_stops_through_the_real_socket() {
    let (d, log) = start_with_music();
    let mut sub = Client::connect(&d.socket);
    sub.data(json!({"type": "subscribe", "topics": ["assistant"]}));
    let mut c = Client::connect(&d.socket);

    // Nothing playing to begin with, and a read is a read.
    let empty = c.data(json!({"type": "music", "op": "show"}));
    assert_eq!(empty["now"]["state"], "stopped");
    assert_eq!(empty["queue"].as_array().unwrap().len(), 0);
    assert!(c.data(json!({"type": "bar_status"})).get("now_playing").is_none());

    // Play something.
    let s = c.data(json!({"type": "music", "op": "play", "query": "hall of fame"}));
    assert_eq!(s["now"]["title"], "hall of fame 0");
    assert_eq!(s["now"]["artist"], "Boards of Canada");
    assert_eq!(s["now"]["state"], "playing");
    assert_eq!(s["now"]["label"], "hall of fame 0 — Boards of Canada");
    assert_eq!(s["controllable"], true);
    // The stream url is never sent to a client.
    assert!(!serde_json::to_string(&s).unwrap().contains("example.invalid/stream"));

    // The playhead is filled in by the supervisor, not by the play request:
    // the request answers immediately and the player has not been asked where
    // it is yet. Wait for a tick, which is also the only thing that proves the
    // supervisor is running at all.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let pos = c.data(json!({"type": "music", "op": "position"}));
        if pos["duration"].as_f64().unwrap_or(0.0) > 0.0 {
            assert_eq!(pos["duration"], 233.0, "the playhead was never picked up");
            break;
        }
        assert!(Instant::now() < deadline, "the supervisor never reported a position: {pos}");
        std::thread::sleep(Duration::from_millis(100));
    }

    let ev = sub.wait_event(|e| e["event"] == "music", Duration::from_secs(5));
    assert_eq!(ev["status"]["now"]["title"], "hall of fame 0");
    assert_eq!(c.data(json!({"type": "bar_status"}))["now_playing"]["title"], "hall of fame 0");

    // The player was actually handed the resolved stream.
    assert!(
        player_log(&log).iter().any(|l| l.contains("example.invalid/stream0")),
        "the player was never asked to load the track: {:?}",
        player_log(&log)
    );

    // Pause reaches the player and the row says paused.
    let s = c.data(json!({"type": "music", "op": "pause"}));
    assert_eq!(s["now"]["state"], "paused");
    assert_eq!(s["now"]["playing"], true, "a paused track must stay on screen");
    let ev = sub.wait_event(
        |e| e["event"] == "music" && e["status"]["now"]["state"] == "paused",
        Duration::from_secs(5),
    );
    assert_eq!(ev["status"]["now"]["title"], "hall of fame 0");

    // Resume.
    assert_eq!(c.data(json!({"type": "music", "op": "resume"}))["now"]["state"], "playing");

    // Queue a second track; it must be appended, not replace what is playing.
    let s = c.data(json!({"type": "music", "op": "enqueue", "query": "teardrop"}));
    assert_eq!(s["now"]["title"], "hall of fame 0", "enqueue changed the playing track");
    assert_eq!(s["queue"][0]["title"], "teardrop 0");
    let ev = sub.wait_event(
        |e| e["event"] == "music" && !e["status"]["queue"].as_array().unwrap().is_empty(),
        Duration::from_secs(5),
    );
    assert_eq!(ev["status"]["queue"][0]["title"], "teardrop 0");

    // Removing queue position 0 must not touch the playing track.
    let s = c.data(json!({"type": "music", "op": "remove", "index": 0}));
    assert_eq!(s["now"]["title"], "hall of fame 0");
    assert_eq!(s["queue"].as_array().unwrap().len(), 0);

    // Stopping kills the player process and clears everything.
    let s = c.data(json!({"type": "music", "op": "stop"}));
    assert_eq!(s["now"]["state"], "stopped");
    assert_eq!(s["queue"].as_array().unwrap().len(), 0);
    let ev = sub.wait_event(
        |e| e["event"] == "music" && e["status"]["now"]["state"] == "stopped",
        Duration::from_secs(5),
    );
    assert_eq!(ev["status"]["now"]["title"], "", "a stop left the old title attached: {ev}");
    assert!(c.data(json!({"type": "bar_status"})).get("now_playing").is_none());
    assert!(player_log(&log).iter().any(|l| l.contains("\"quit\"")));

    // And with nothing playing, the transport controls refuse rather than
    // reporting a success that did nothing.
    let v = c.call(json!({"type": "music", "op": "next"}));
    assert_eq!(v["status"], "error");
}

/// Controls with nothing playing are errors, not silent successes.
#[test]
fn music_controls_refuse_when_nothing_is_playing() {
    let (d, _log) = start_with_music();
    let mut c = Client::connect(&d.socket);
    for op in ["pause", "resume", "next", "previous"] {
        let v = c.call(json!({"type": "music", "op": op}));
        assert_eq!(v["status"], "error", "`{op}` on silence reported success");
        assert!(v["error"]["message"].as_str().unwrap().contains("nothing"), "{v}");
    }
}

/// A query with nothing to search for is refused before any process starts.
#[test]
fn music_refuses_an_empty_query() {
    let (d, log) = start_with_music();
    let mut c = Client::connect(&d.socket);
    let v = c.call(json!({"type": "music", "op": "play", "query": "   "}));
    assert_eq!(v["status"], "error");
    assert!(player_log(&log).is_empty(), "a player was started for an empty query");
}


/// The daemon owns the player, so the player must not outlive it: on shutdown
/// the process it started is gone.
#[test]
fn the_player_does_not_outlive_the_daemon() {
    let (d, log) = start_with_music();
    let mut c = Client::connect(&d.socket);
    c.data(json!({"type": "music", "op": "play", "query": "gate"}));
    let pid = c.data(json!({"type": "music", "op": "show"}))["now"]["pid"].as_u64();
    assert!(pid.is_some_and(|p| p > 0), "no player pid was reported");
    assert!(unsafe { libc::kill(pid.unwrap() as i32, 0) } == 0, "the player is not running");
    drop(d);
    // SIGTERM is async; the process gets a moment to exit on its own.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if unsafe { libc::kill(pid.unwrap() as i32, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the player outlived the daemon that started it; it was asked: {:?}", player_log(&log));
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
