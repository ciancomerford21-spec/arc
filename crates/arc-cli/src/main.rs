//! `arc` — command-line client for the Arc daemon.
//!
//! Synchronous std-only socket client (no async runtime): starts in a few
//! milliseconds, which matters because bar widgets call it on every click.

use anyhow::{Context, Result, bail};
use arc_proto::socket_path;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

// `watch` and `bar --follow` stream into whatever launched them (the Arc app,
// the bar widget). When that reader goes away, std's println! panics on the
// broken pipe, which shows up as a crash report every time the app is closed.
// A closed stdout just means nobody is listening any more: exit quietly.
macro_rules! println {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        if writeln!(out, $($t)*).and_then(|_| out.flush()).is_err() {
            std::process::exit(0);
        }
    }};
}

#[derive(Parser)]
#[command(name = "arc", version, about = "Talk to the Arc assistant")]
struct Cli {
    /// Daemon socket (default: $XDG_RUNTIME_DIR/arc/arc.sock).
    #[arg(long, env = "ARC_SOCKET", global = true)]
    socket: Option<PathBuf>,
    /// Print raw JSON responses.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Ask Arc something: `arc ask set volume to 30`.
    Ask {
        #[arg(required = true, trailing_var_arg = true)]
        text: Vec<String>,
        /// Also speak the reply.
        #[arg(long)]
        speak: bool,
    },
    /// Approve a pending action (default: the most recent one).
    Confirm { id: Option<String> },
    /// Reject a pending action (default: the most recent one).
    Reject { id: Option<String> },
    /// Daemon and component status.
    Status,
    /// List tools with their risk levels.
    Tools,
    /// Change a tool's safety classification: `arc tool reboot dangerous`.
    ///
    /// The same thing the Arc app's dropdown does; the app shells out to this
    /// rather than opening a socket itself. `default` restores the tool's own
    /// level, and `show` prints it without changing anything.
    Tool {
        name: String,
        #[arg(default_value = "show", value_name = "LEVEL")]
        level: String,
    },
    /// Voice control: start | stop | toggle | cancel | stop-speaking | say <text>.
    Voice {
        action: String,
        #[arg(trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// Bar status. `--waybar` prints Waybar JSON; `--follow` keeps printing on change.
    Bar {
        #[arg(long)]
        waybar: bool,
        #[arg(long)]
        follow: bool,
    },
    /// Playback: `arc music <action>`.
    ///
    /// The daemon owns the player, so these act on the real thing rather than
    /// reporting a status nobody can change:
    ///
    ///   show                    what is playing and what is queued (the default)
    ///   play <query>            resolve and play, replacing the queue
    ///   enqueue <query>         resolve and add to the end of the queue
    ///   pause | resume | toggle stop, start or flip playback
    ///   next | previous         skip through the queue
    ///   stop                    stop playback and empty the queue
    ///   clear                   empty the queue, keep playing
    ///   remove <n>              drop the nth queued track
    ///   position                the playhead, for a progress bar
    ///
    /// `position` is separate from `show` on purpose: it is the only part of the
    /// status that changes on its own, so a client can poll it once a second
    /// without also re-reading and re-rendering the whole queue.
    Music {
        #[arg(value_name = "ACTION", default_value = "show")]
        action: String,
        /// What to search for (with `play` and `enqueue`), or which queued track
        /// to drop (with `remove`), counting from 0 after the track that is
        /// playing.
        ///
        /// One positional, not two. With `query` and `index` as separate optional
        /// positionals, clap filled the first with whatever came next, so
        /// `arc music remove 0` put "0" in `query` and then refused for a
        /// missing index.
        #[arg(value_name = "QUERY_OR_INDEX")]
        arg: Option<String>,
    },
    /// Stream daemon events (replies, state changes, confirmations).
    Watch {
        /// Topics: assistant, desktop, voice_activity, errors.
        #[arg(long, value_delimiter = ',', default_value = "assistant,errors")]
        topics: Vec<String>,
    },
    /// Forget the short-term conversation.
    Reset,
    /// Inspect and edit remembered facts: `arc memory list|search|forget|clear`.
    Memory {
        #[arg(value_name = "ACTION", default_value = "list")]
        action: String,
        /// Text for `search`, or an id for `forget`.
        #[arg(trailing_var_arg = true)]
        arg: Vec<String>,
    },
    /// Show the Arc panel (the Omarchy bar widget's popover).
    Panel {
        #[arg(long)]
        toggle: bool,
    },
    /// Check the daemon is reachable.
    Ping,
}

struct Conn {
    w: UnixStream,
    r: BufReader<UnixStream>,
    next: u64,
}

impl Conn {
    fn open(path: &PathBuf, timeout: Option<Duration>) -> Result<Self> {
        let s = UnixStream::connect(path).with_context(|| {
            format!("cannot reach the Arc daemon at {} (is arcd running?)", path.display())
        })?;
        s.set_read_timeout(timeout)?;
        Ok(Self { r: BufReader::new(s.try_clone()?), w: s, next: 1 })
    }

    fn send(&mut self, mut req: Value) -> Result<u64> {
        let id = self.next;
        self.next += 1;
        req["id"] = json!(id);
        writeln!(self.w, "{req}")?;
        Ok(id)
    }

    fn line(&mut self) -> Result<Value> {
        let mut l = String::new();
        if self.r.read_line(&mut l)? == 0 {
            bail!("the daemon closed the connection");
        }
        Ok(serde_json::from_str(&l)?)
    }

    /// Request → `data`, or an error with the daemon's message.
    fn call(&mut self, req: Value) -> Result<Value> {
        let id = self.send(req)?;
        loop {
            let v = self.line()?;
            if v["kind"] == "response" && v["id"] == id {
                if v["status"] == "ok" {
                    return Ok(v["data"].clone());
                }
                bail!("{}", v["error"]["message"].as_str().unwrap_or("request failed"));
            }
        }
    }
}

fn print_ask(r: &Value, json_out: bool) {
    if json_out {
        println!("{r}");
        return;
    }
    println!("{}", r["reply"].as_str().unwrap_or(""));
    if let Some(p) = r.get("pending").filter(|p| !p.is_null()) {
        eprintln!(
            "\n  pending [{}] {} — confirm with: arc confirm {}",
            p["risk"].as_str().unwrap_or("?"),
            p["explanation"].as_str().unwrap_or(""),
            p["confirmation_id"].as_str().unwrap_or("")
        );
    }
}

fn waybar(b: &Value) -> Value {
    json!({
        "text": b["text"],
        "tooltip": b["tooltip"],
        "class": b["class"].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>(),
        "alt": b["state"],
    })
}

fn offline_bar(waybar_fmt: bool) -> Value {
    let b =
        json!({"state": "offline", "text": "󰍭", "tooltip": "Arc daemon is not running", "class": "offline"});
    if waybar_fmt { waybar(&b) } else { b }
}

/// Without an id, approve/reject the most recent pending action — the same
/// path a spoken "yes"/"no" takes (UI source, so dangerous actions are
/// allowed: the user is at the keyboard).
fn resolve_latest(conn: &mut Conn, approve: bool) -> Result<Value> {
    conn.call(json!({"type": "ask", "text": if approve { "yes" } else { "no" }, "source": "ui"}))
}

/// Is `name` an executable on PATH? Used to pick a shell entry point without
/// assuming which of them is installed.
fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// The search string for `play`/`enqueue`, or a message naming the argument
/// that is missing rather than letting an empty search run.
fn query_arg(action: &str, query: &Option<String>) -> Result<String> {
    match query.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        Some(q) => Ok(q.to_string()),
        None => bail!(
            "`arc music {action}` needs something to search for: `arc music {action} \"boards of canada\"`"
        ),
    }
}

/// What a person wants to read after asking for a playback action.
///
/// Deliberately not a JSON dump: these are the words the assistant and the
/// user both see in a terminal. Every line here is something that actually
/// happened -- a refused control says so rather than printing an empty status.
fn print_music(action: &str, out: &Value) {
    let now = &out["now"];
    let label = now["label"].as_str().unwrap_or("");
    let queued = out["queue"].as_array().map(|q| q.len()).unwrap_or(0);
    match action {
        "show" => {
            if now["playing"] != true {
                println!("nothing playing");
                return;
            }
            println!(
                "{}",
                if now["state"] == "paused" { format!("{label}  (paused)") } else { label.to_string() }
            );
            for t in out["queue"].as_array().into_iter().flatten() {
                // A queued track carries a title and an artist, not the
                // combined `label` the current track has. Reading `label` here
                // printed an empty line under every queued entry.
                let title = t["title"].as_str().unwrap_or("");
                let artist = t["artist"].as_str().unwrap_or("");
                println!(
                    "  queued: {title}{}",
                    if artist.is_empty() { String::new() } else { format!(" — {artist}") }
                );
            }
        }
        "position" => {
            println!(
                "{:.0}s / {:.0}s",
                out["position"].as_f64().unwrap_or(0.0),
                out["duration"].as_f64().unwrap_or(0.0)
            );
        }
        "play" | "enqueue" => {
            println!("{label}");
            if queued > 0 {
                println!("{queued} more queued");
            }
        }
        "stop" => println!("stopped"),
        "clear" => println!("queue cleared"),
        _ => println!("{label}"),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let sock = cli.socket.clone().unwrap_or_else(socket_path);
    match cli.cmd {
        Cmd::Ping => {
            let r = Conn::open(&sock, Some(Duration::from_secs(5)))?.call(json!({"type": "ping"}))?;
            println!("arcd {} (protocol {})", r["version"].as_str().unwrap_or("?"), r["protocol"]);
        }
        Cmd::Ask { text, speak } => {
            let mut c = Conn::open(&sock, Some(Duration::from_secs(900)))?;
            let source = if speak { "voice" } else { "text" };
            let r = c.call(json!({"type": "ask", "text": text.join(" "), "source": source}))?;
            print_ask(&r, cli.json);
        }
        Cmd::Confirm { id } => {
            let mut c = Conn::open(&sock, Some(Duration::from_secs(900)))?;
            let r = match id {
                Some(id) => c.call(json!({"type": "confirm", "confirmation_id": id, "approve": true}))?,
                None => resolve_latest(&mut c, true)?,
            };
            print_ask(&r, cli.json);
        }
        Cmd::Reject { id } => {
            let mut c = Conn::open(&sock, Some(Duration::from_secs(30)))?;
            let r = match id {
                Some(id) => c.call(json!({"type": "confirm", "confirmation_id": id, "approve": false}))?,
                None => resolve_latest(&mut c, false)?,
            };
            print_ask(&r, cli.json);
        }
        Cmd::Status => {
            let s = Conn::open(&sock, Some(Duration::from_secs(5)))?.call(json!({"type": "status"}))?;
            if cli.json {
                println!("{s}");
                return Ok(());
            }
            println!(
                "Arc {}  pid {}  up {}s  state {}",
                s["version"].as_str().unwrap_or("?"),
                s["pid"],
                s["uptime_s"],
                s["state"].as_str().unwrap_or("?")
            );
            println!(
                "AI: {} {}",
                s["ai_provider"].as_str().unwrap_or(""),
                s["ai_model"].as_str().unwrap_or("")
            );
            println!("Voice mode: {}", s["voice_mode"].as_str().unwrap_or("off"));
            println!("Tools: {}/{} enabled", s["tools_enabled"], s["tools_total"]);
            for c in s["components"].as_array().into_iter().flatten() {
                println!(
                    "  {:<12} {:<12} {}",
                    c["name"].as_str().unwrap_or(""),
                    c["status"].as_str().unwrap_or(""),
                    c["detail"].as_str().unwrap_or("")
                );
            }
            if let Some(e) = s["recent_errors"].as_array().filter(|e| !e.is_empty()) {
                println!("Recent errors:");
                for x in e {
                    println!("  {}", x.as_str().unwrap_or(""));
                }
            }
        }
        Cmd::Tools => {
            let t = Conn::open(&sock, Some(Duration::from_secs(5)))?.call(json!({"type": "tools"}))?;
            if cli.json {
                println!("{t}");
                return Ok(());
            }
            for x in t.as_array().into_iter().flatten() {
                println!(
                    "{:<20} {:<10} {}{}{}",
                    x["name"].as_str().unwrap_or(""),
                    x["risk"].as_str().unwrap_or(""),
                    x["description"].as_str().unwrap_or(""),
                    if x["enabled"] == false { "  (disabled)" } else { "" },
                    // Say so when the user has overridden the tool's own level,
                    // and what they overrode: otherwise the list disagrees with
                    // the code and there is no way to tell why.
                    if x["reclassified"] == true {
                        format!("  [built-in {}]", x["default_risk"].as_str().unwrap_or("?"))
                    } else {
                        String::new()
                    }
                );
            }
        }
        Cmd::Tool { name, level } => {
            // `show` is a read, so it must not need a level argument at all --
            // the app uses it to render a row before the user has clicked.
            let body = match level.as_str() {
                "show" => json!({"type": "tools"}),
                "default" | "reset" => {
                    json!({"type": "set_tool_class", "tool": name, "level": Value::Null})
                }
                "safe" | "caution" | "dangerous" => {
                    json!({"type": "set_tool_class", "tool": name, "level": level})
                }
                other => bail!("unknown classification `{other}` (safe, caution, dangerous, default, show)"),
            };
            let r = Conn::open(&sock, Some(Duration::from_secs(5)))?.call(body)?;
            if cli.json {
                println!("{r}");
                return Ok(());
            }
            if level == "show" {
                // Print just this tool's row.
                if let Some(x) =
                    r.as_array().and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(name.as_str())))
                {
                    println!(
                        "{:<20} {:<10} {}{}",
                        x["name"].as_str().unwrap_or(""),
                        x["risk"].as_str().unwrap_or(""),
                        if x["enabled"] == false { "  (disabled)" } else { "" },
                        if x["reclassified"] == true {
                            format!("  (built-in {})", x["default_risk"].as_str().unwrap_or("?"))
                        } else {
                            String::new()
                        }
                    );
                } else {
                    bail!("no such tool `{name}`");
                }
                return Ok(());
            }
            println!("{name} is now {}.", r["risk"].as_str().unwrap_or("unchanged"));
        }
        Cmd::Voice { action, text } => {
            let command = match action.as_str() {
                "start" => json!({"action": "start_listening"}),
                "stop" => json!({"action": "stop_listening"}),
                "toggle" => json!({"action": "toggle_listening"}),
                "cancel" => json!({"action": "cancel_listening"}),
                "stop-speaking" | "shush" => json!({"action": "stop_speaking"}),
                "say" => json!({"action": "speak", "text": text.join(" "), "utterance_id": "cli"}),
                "mode" => {
                    json!({"action": "set_mode", "mode": text.first().cloned().unwrap_or_default().replace('-', "_")})
                }
                other => bail!(
                    "unknown voice action `{other}` (start, stop, toggle, cancel, stop-speaking, say, mode)"
                ),
            };
            Conn::open(&sock, Some(Duration::from_secs(5)))?
                .call(json!({"type": "voice", "command": command}))?;
        }
        Cmd::Bar { waybar: wb, follow } => {
            if !follow {
                let out = match Conn::open(&sock, Some(Duration::from_secs(2)))
                    .and_then(|mut c| c.call(json!({"type": "bar_status"})))
                {
                    Ok(b) => {
                        if wb {
                            waybar(&b)
                        } else {
                            b
                        }
                    }
                    Err(_) => offline_bar(wb),
                };
                println!("{out}");
                return Ok(());
            }
            // Follow mode (Waybar "return-type": "json" + long-running exec):
            // print one line per state change; survive daemon restarts.
            loop {
                let res: Result<()> = (|| {
                    let mut c = Conn::open(&sock, None)?;
                    let b = c.call(json!({"type": "bar_status"}))?;
                    println!("{}", if wb { waybar(&b) } else { b });
                    c.call(json!({"type": "subscribe", "topics": ["assistant"]}))?;
                    loop {
                        let v = c.line()?;
                        if v["kind"] == "event" && v["event"] == "bar" {
                            let b = &v["status"];
                            println!("{}", if wb { waybar(b) } else { b.clone() });
                        }
                    }
                })();
                let _ = res;
                println!("{}", offline_bar(wb));
                std::thread::sleep(Duration::from_secs(3));
            }
        }
        Cmd::Music { action, arg } => {
            let req = match action.as_str() {
                "show" | "get" => json!({"type": "music", "op": "show"}),
                "position" => json!({"type": "music", "op": "position"}),
                "play" => json!({"type": "music", "op": "play", "query": query_arg(&action, &arg)?}),
                "enqueue" | "queue" | "add" => {
                    json!({"type": "music", "op": "enqueue", "query": query_arg(&action, &arg)?})
                }
                "pause" => json!({"type": "music", "op": "pause"}),
                "resume" => json!({"type": "music", "op": "resume"}),
                "toggle" => json!({"type": "music", "op": "toggle"}),
                "next" | "skip" => json!({"type": "music", "op": "next"}),
                "previous" | "back" => json!({"type": "music", "op": "previous"}),
                "stop" => json!({"type": "music", "op": "stop"}),
                "clear" => json!({"type": "music", "op": "clear"}),
                "remove" => {
                    // Parsed from the one positional rather than taken as a
                    // second optional positional: with two, clap handed
                    // `remove 0`'s "0" to `query` and this arm always saw
                    // `index` as None.
                    let index = arg
                        .as_deref()
                        .map(str::trim)
                        .filter(|a| !a.is_empty())
                        .and_then(|a| a.parse::<usize>().ok())
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "`arc music remove` needs which queued track to drop, counted from \
                                 0 after the one playing: `arc music remove 0`"
                            )
                        })?;
                    json!({"type": "music", "op": "remove", "index": index})
                }
                "seek" => {
                    // Parsed from the one positional, like `remove`, and
                    // rejected loudly rather than passed on as NaN: `seek`
                    // with no argument used to be a silent no-op waiting to
                    // happen if it had defaulted to 0.
                    let secs = arg
                        .as_deref()
                        .map(str::trim)
                        .filter(|a| !a.is_empty())
                        .and_then(|a| a.parse::<f64>().ok())
                        .filter(|s| s.is_finite() && *s >= 0.0)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "`arc music seek` needs a position in seconds: `arc music seek 90`"
                            )
                        })?;
                    json!({"type": "music", "op": "seek", "seconds": secs})
                }
                other => bail!(
                    "unknown music action `{other}` (show, position, play, enqueue, pause, \
                     resume, toggle, next, previous, seek, stop, clear, remove)"
                ),
            };
            // Reading must work without the daemon: a widget asking "what is
            // playing?" should get "nothing" rather than an error dialog, and
            // stopping is local state, so an unreachable daemon counts as
            // already stopped.
            let out = match Conn::open(&sock, Some(Duration::from_secs(60))).and_then(|mut c| c.call(req)) {
                Ok(v) => v,
                Err(_) if action == "stop" => json!({
                    "now": {"state": "stopped", "title": "", "artist": "", "label": "", "playing": false},
                    "queue": [],
                }),
                Err(e) => return Err(e),
            };
            if cli.json {
                println!("{out}");
                return Ok(());
            }
            print_music(&action, &out);
        }
        Cmd::Watch { topics } => {
            let mut c = Conn::open(&sock, None)?;
            c.call(json!({"type": "subscribe", "topics": topics}))?;
            loop {
                let v = c.line()?;
                if v["kind"] != "event" {
                    continue;
                }
                if cli.json {
                    println!("{v}");
                    continue;
                }
                match v["event"].as_str().unwrap_or("") {
                    "state" => println!("[{}]", v["state"].as_str().unwrap_or("")),
                    "heard" => println!("heard: {}", v["text"].as_str().unwrap_or("")),
                    "reply" => println!("arc:   {}", v["text"].as_str().unwrap_or("")),
                    "thought" => {
                        for key in ["reasoning", "text"] {
                            let t = v[key].as_str().unwrap_or("").trim();
                            if !t.is_empty() {
                                println!("think: {}", t.replace('\n', " "));
                            }
                        }
                    }
                    "tool_started" => println!("tool:  {} {}", v["tool"].as_str().unwrap_or(""), v["args"]),
                    "tool_finished" => println!(
                        "tool:  {} -> {}",
                        v["record"]["tool"].as_str().unwrap_or(""),
                        v["record"]["outcome"].as_str().unwrap_or("")
                    ),
                    "confirmation_required" => println!(
                        "confirm? {} (arc confirm {})",
                        v["pending"]["explanation"].as_str().unwrap_or(""),
                        v["pending"]["confirmation_id"].as_str().unwrap_or("")
                    ),
                    "error" => println!(
                        "error: {}: {}",
                        v["component"].as_str().unwrap_or(""),
                        v["message"].as_str().unwrap_or("")
                    ),
                    _ => println!("{v}"),
                }
            }
        }
        Cmd::Reset => {
            Conn::open(&sock, Some(Duration::from_secs(5)))?.call(json!({"type": "reset_conversation"}))?;
            println!("Conversation cleared.");
        }
        Cmd::Memory { action, arg } => {
            let mut c = Conn::open(&sock, Some(Duration::from_secs(5)))?;
            let join = arg.join(" ");
            let req = match action.as_str() {
                "list" => json!({"type": "memory", "op": "list"}),
                "search" => {
                    if join.trim().is_empty() {
                        bail!("`arc memory search` needs something to search for");
                    }
                    json!({"type": "memory", "op": "list", "category": join})
                }
                "remember" => {
                    // `arc memory remember key = value`
                    let Some((k, v)) = join.split_once('=') else {
                        bail!("usage: arc memory remember <key> = <value>");
                    };
                    if k.trim().is_empty() || v.trim().is_empty() {
                        bail!("both sides of `=` must be non-empty");
                    }
                    json!({"type": "memory", "op": "remember", "category": "general", "key": k.trim(), "value": v.trim()})
                }
                "forget" => {
                    if join.trim().is_empty() {
                        bail!("`arc memory forget` needs an id or key");
                    }
                    json!({"type": "memory", "op": "forget", "key": join.trim()})
                }
                "forget-last" => json!({"type": "memory", "op": "forget_last"}),
                "clear" => json!({"type": "memory", "op": "clear"}),
                other => bail!(
                    "unknown memory action `{other}` (list, search, remember, forget, forget-last, clear)"
                ),
            };
            let r = c.call(req)?;
            if cli.json {
                println!("{r}");
                return Ok(());
            }
            match action.as_str() {
                "list" | "search" => {
                    let facts = r["facts"].as_array().cloned().unwrap_or_default();
                    if facts.is_empty() {
                        println!("Nothing remembered yet.");
                    }
                    for f in facts {
                        println!("{}  {}", f["id"].as_str().unwrap_or("?"), f["fact"].as_str().unwrap_or(""));
                    }
                }
                "remember" => println!("Noted: {}.", r["fact"].as_str().unwrap_or("")),
                "forget" | "forget-last" => println!("Forgot {}.", r["forgotten"].as_str().unwrap_or("it")),
                "clear" => println!("Forgot {} fact(s).", r["cleared"].as_u64().unwrap_or(0)),
                _ => println!("{r}"),
            }
        }
        Cmd::Panel { toggle } => {
            // The panel is the Omarchy bar widget's popover, not a separate
            // process: `omarchy-launch-or-focus-tui` brings up the shell's own
            // surface. With no widget running there is nothing to show, so say
            // so rather than spawning a binary that no longer exists.
            let _ = toggle;
            let candidates = ["omarchy-launch-or-focus-tui", "quickshell"];
            let mut launched = false;
            for exe in candidates {
                if which(exe).is_none() {
                    continue;
                }
                let mut cmd = std::process::Command::new(exe);
                if exe == "omarchy-launch-or-focus-tui" {
                    cmd.args(["--", "qs", "-c", "arc.panel"]);
                }
                cmd.stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .with_context(|| format!("cannot start {exe}"))?;
                launched = true;
                break;
            }
            if !launched {
                eprintln!("the Arc panel is part of the Omarchy bar widget; is the shell running?");
            }
        }
    }
    Ok(())
}
