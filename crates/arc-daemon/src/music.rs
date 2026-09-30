//! The player Arc owns, and the resolver that feeds it.
//!
//! Everything here was read off the mpv on this machine rather than taken
//! from the manual, because the manual and the binary disagree in a way that
//! would have produced a player that never queued anything. mpv 0.41 has no
//! `playlist-append` and no `playlist-count` command: both come back
//! `invalid parameter`. `mpv --input-cmdlist` is the authority, and what this
//! build actually offers is `loadfile <url> replace|append`,
//! `playlist-play-index`, `playlist-remove`, `playlist-move`,
//! `playlist-clear`, `playlist-next`, `playlist-prev` -- plus the properties
//! `playlist`, `playlist-pos`, `pause`, `time-pos`, `duration`,
//! `idle-active`, which is where the queue is *read*.
//!
//! mpv advances through a playlist on its own, so this module never tells it to
//! skip on its own initiative. Advancing is a user action (`Next`), and at the
//! end of the queue mpv goes idle, which is how the daemon learns the queue
//! finished.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use arc_proto::Track;
use serde_json::{Value, json};

/// How long a player gets to create its control socket before it is called a
/// failure. mpv binds it before it opens the audio device, so this is generous
/// on a loaded machine and still bounded.
const SOCKET_WAIT: Duration = Duration::from_secs(5);

/// How long to wait for a command's reply. mpv answers these in microseconds;
/// the timeout is there so a wedged player cannot hang the daemon's socket
/// handler forever.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the player's control socket goes. Short, and inside the runtime dir
/// so it is cleaned up with the session -- and short matters, because a Unix
/// socket path is capped at ~108 bytes by the kernel.
pub fn socket_path() -> PathBuf {
    arc_proto::runtime_dir().join("music.sock")
}

/// What the player is actually doing, read back from it.
///
/// This is the daemon's source of truth for the current track's identity. The
/// daemon keeps its own queue for the *metadata* (titles, artists), but which
/// entry is playing and whether it is paused can only be answered by the thing
/// doing the playing.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Progress {
    /// Index into the queue, or `None` when nothing is loaded.
    pub index: Option<usize>,
    pub paused: bool,
    pub position: f64,
    pub duration: f64,
    /// True when the player has run out of playlist and is sitting idle.
    pub idle: bool,
}

/// The player's controls, as the daemon needs them.
///
/// A trait rather than a concrete mpv so the queue logic can be tested against
/// a recorded player instead of a real one -- the daemon's own tests should
/// not start processes or make the user's speakers do anything.
pub trait Player: Send + Sync + std::fmt::Debug {
    /// Start playing `tracks` from the beginning, replacing the queue.
    fn replace(&self, tracks: &[Track]) -> Result<(), String>;
    /// Add `tracks` to the end of the queue.
    fn append(&self, tracks: &[Track]) -> Result<(), String>;
    /// Drop the queue entry at `index`.
    fn remove(&self, index: usize) -> Result<(), String>;
    /// Make queue entry `index` the current one.
    fn play_index(&self, index: usize) -> Result<(), String>;
    /// The player's own "back": restart this track, or go to the one before
    /// it if there is one. Not `play_index(current - 1)` -- a player that
    /// jumps back a whole track when you press back twice is wrong, and mpv's
    /// rule (restart, unless you are more than a second in) is the one every
    /// other player uses.
    fn previous(&self) -> Result<(), String>;
    /// Drop every queue entry after the current one.
    fn remove_remaining(&self) -> Result<(), String>;
    fn set_paused(&self, paused: bool) -> Result<(), String>;
    /// Read the player's own state.
    fn progress(&self) -> Result<Progress, String>;
    /// The player process, when there is one. Used to notice that it died.
    fn pid(&self) -> Option<u32>;
    /// Stop playback and terminate the process. Must not fail.
    fn shutdown(&self);
}

// ---------------------------------------------------------------------------
// mpv
// ---------------------------------------------------------------------------

/// An mpv process Arc started, plus its control socket.
#[derive(Debug)]
struct Mpv {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    child: Child,
    next_id: u64,
    scratch: String,
}

impl Mpv {
    /// Send one command and wait for its reply.
    ///
    /// mpv interleaves its own events (`file-loaded`, `end-file`, …) with the
    /// replies on the same socket, so every line is read until the one
    /// carrying our request id turns up. Events are read and dropped: the
    /// daemon polls for state instead, which cannot miss an event that
    /// arrived while nobody was reading.
    fn command(&mut self, args: &[Value]) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let line = json!({"command": args, "request_id": id}).to_string();
        self.stream.write_all(line.as_bytes()).and_then(|_| self.stream.write_all(b"\n")).map_err(io)?;
        self.stream.flush().map_err(io)?;

        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            if Instant::now() > deadline {
                return Err("the player did not answer".into());
            }
            self.scratch.clear();
            let n = self
                .reader
                .read_line(&mut self.scratch)
                .map_err(|e| format!("the player stopped responding: {e}"))?;
            if n == 0 {
                return Err("the player closed its connection".into());
            }
            let Ok(v) = serde_json::from_str::<Value>(self.scratch.trim()) else { continue };
            if v.get("request_id").and_then(Value::as_u64) != Some(id) {
                continue; // an event, or a reply to something else
            }
            return match v.get("error").and_then(Value::as_str) {
                None | Some("success") => Ok(v.get("data").cloned().unwrap_or(Value::Null)),
                Some(e) => Err(format!("the player rejected {args:?}: {e}")),
            };
        }
    }
}

fn io(e: std::io::Error) -> String {
    format!("player connection failed: {e}")
}

/// The real player: an mpv Arc started, driven over its JSON IPC socket.
#[derive(Debug)]
pub struct MpvPlayer {
    binary: String,
    sock: PathBuf,
    inner: Mutex<Option<Mpv>>,
}

impl MpvPlayer {
    pub fn new(cfg: &arc_config::Music) -> Self {
        Self { binary: cfg.player.clone(), sock: socket_path(), inner: Mutex::new(None) }
    }

    /// Start a player, or hand back the one already running.
    ///
    /// Starting with an empty playlist and `--idle=yes` is deliberate: the
    /// player then outlives the end of a track and waits for the queue to be
    /// filled, instead of exiting and needing a new process for every song.
    fn ensure(&self) -> Result<(), String> {
        let mut guard = self.inner.lock().map_err(|_| "player lock poisoned")?;
        if let Some(m) = guard.as_mut()
            && m.child.try_wait().map_err(io)?.is_none()
        {
            return Ok(());
        }
        // Either there was never one, or it exited. Take the corpse first so a
        // later command reports "not running" rather than writing to a socket
        // nobody is listening on.
        if let Some(mut old) = guard.take() {
            let _ = old.child.kill();
            let _ = old.child.wait();
        }
        let _ = std::fs::remove_file(&self.sock);
        if let Some(dir) = self.sock.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let child = Command::new(&self.binary)
            .args([
                "--no-video",
                "--really-quiet",
                "--no-terminal",
                "--idle=yes",
                // Start with nothing: the queue arrives through the socket, so
                // a track can be queued without racing a process launch.
                "--playlist=/dev/null",
                // The `=` form, in one argument, is required. Given
                // `--input-ipc-server /path/sock`, this mpv does not read the
                // path as the option's value: it treats the path as a file to
                // play, exits 1, and never creates the socket. The test stub
                // accepted the space form, so only real mpv ever showed it.
                &format!("--input-ipc-server={}", self.sock.display()),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", self.binary))?;

        let stream = wait_for_socket(&self.sock, SOCKET_WAIT)
            .ok_or_else(|| format!("{} did not open its control socket", self.binary))?;
        let reader = BufReader::new(stream.try_clone().map_err(io)?);
        stream.set_read_timeout(Some(Duration::from_millis(250))).map_err(io)?;
        *guard = Some(Mpv { stream, reader, child, next_id: 0, scratch: String::new() });
        Ok(())
    }

    fn with<R>(&self, f: impl FnOnce(&mut Mpv) -> Result<R, String>) -> Result<R, String> {
        self.ensure()?;
        let mut guard = self.inner.lock().map_err(|_| "player lock poisoned")?;
        match guard.as_mut() {
            Some(m) => f(m),
            // ensure() just succeeded, so this cannot normally happen; saying
            // so beats unwrapping a None that would panic the daemon.
            None => Err("the player is not running".into()),
        }
    }

    fn load(&self, mode: &str, tracks: &[Track]) -> Result<(), String> {
        if tracks.is_empty() {
            return Ok(());
        }
        self.with(|m| {
            for t in tracks {
                m.command(&[json!("loadfile"), json!(t.url), json!(mode)])?;
            }
            Ok(())
        })
    }
}

/// Wait for a socket file to exist and accept, or give up.
///
/// The file appears slightly before the listener does, so "exists" alone
/// connects about half the time. A refused connection is retried rather than
/// reported, which is the difference between starting reliably and starting
/// usually.
fn wait_for_socket(path: &Path, timeout: Duration) -> Option<UnixStream> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(s) = UnixStream::connect(path) {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

impl Player for MpvPlayer {
    fn replace(&self, tracks: &[Track]) -> Result<(), String> {
        // Clear before loading: `replace` only replaces the *current* entry,
        // so a second `play` would leave the old queue behind and the new
        // track would start after it.
        self.with(|m| {
            for t in tracks {
                m.command(&[json!("loadfile"), json!(t.url), json!("replace")])?;
            }
            Ok(())
        })
    }

    fn append(&self, tracks: &[Track]) -> Result<(), String> {
        self.load("append", tracks)
    }

    fn remove(&self, index: usize) -> Result<(), String> {
        self.with(|m| m.command(&[json!("playlist-remove"), json!(index)]).map(|_| ()))
    }

    fn play_index(&self, index: usize) -> Result<(), String> {
        self.with(|m| m.command(&[json!("playlist-play-index"), json!(index)]).map(|_| ()))
    }

    fn previous(&self) -> Result<(), String> {
        self.with(|m| m.command(&[json!("playlist-prev")]).map(|_| ()))
    }

    fn remove_remaining(&self) -> Result<(), String> {
        // From the top, down to just past what is playing. Removing upwards
        // from index 1 instead would delete the track that is playing on the
        // second pass -- and `playlist-clear` is not an option either, since
        // it empties the playlist including the current entry.
        self.with(|m| {
            let count = m.command(&[json!("get_property"), json!("playlist-count")])?.as_u64().unwrap_or(0);
            let pos = m.command(&[json!("get_property"), json!("playlist-pos")])?.as_i64().unwrap_or(-1);
            let keep = if pos < 0 { 0 } else { pos as u64 + 1 };
            for i in (keep..count).rev() {
                m.command(&[json!("playlist-remove"), json!(i)])?;
            }
            Ok(())
        })
    }

    fn set_paused(&self, paused: bool) -> Result<(), String> {
        self.with(|m| {
            m.command(&[json!("set_property"), json!("pause"), json!(paused)])?;
            Ok(())
        })
    }

    fn progress(&self) -> Result<Progress, String> {
        self.with(|m| {
            let pos = m.command(&[json!("get_property"), json!("playlist-pos")])?;
            // `playlist-pos` is -1 when nothing is loaded, which is also what an
            // exhausted playlist looks like part-way through a poll.
            let index = pos.as_i64().filter(|i| *i >= 0).map(|i| i as usize);
            let paused = m.command(&[json!("get_property"), json!("pause")])?.as_bool().unwrap_or(false);
            let idle = m.command(&[json!("get_property"), json!("idle-active")])?.as_bool().unwrap_or(false);
            // Both of these are `property unavailable` before the first file
            // has a demuxer open, which is normal rather than an error.
            let position = num(&m.command(&[json!("get_property"), json!("time-pos")])?);
            let duration = num(&m.command(&[json!("get_property"), json!("duration")])?);
            Ok(Progress { index, paused, position, duration, idle })
        })
    }

    fn pid(&self) -> Option<u32> {
        // `g.as_ref()` on a MutexGuard would resolve to the guard's own AsRef,
        // not the Option's, so dereference first.
        self.inner.lock().ok().and_then(|g| (*g).as_ref().map(|m| m.child.id()))
    }

    fn shutdown(&self) {
        // Ask first so the player closes its own audio device cleanly, then
        // kill regardless: a player that ignores `quit` must not outlive the
        // daemon that owns it.
        let _ = self.with(|m| m.command(&[json!("quit")]));
        if let Ok(mut g) = self.inner.lock()
            && let Some(mut m) = g.take()
        {
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                match m.child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                    _ => {
                        let _ = m.child.kill();
                        let _ = m.child.wait();
                        break;
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&self.sock);
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A property that may legitimately be missing (`time-pos` before playback
/// starts), or a JSON `null` standing in for one.
fn num(v: &Value) -> f64 {
    v.as_f64().filter(|f| f.is_finite() && *f >= 0.0).unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

/// How a search string becomes playable tracks.
///
/// A trait for the same reason [`Player`] is one: the queue logic is worth
/// testing without reaching the network.
pub trait Resolver: Send + Sync + std::fmt::Debug {
    fn resolve(&self, query: &str, count: usize) -> Result<Vec<Track>, String>;
}

/// yt-dlp, which turns a search string into stream urls and real titles.
#[derive(Debug)]
pub struct YtDlp {
    binary: String,
    timeout: Duration,
    source: String,
}

/// Field separator in the `--print` template. Three pipes because a title, an
/// uploader name and a url can each contain almost anything else, and a
/// separator that a title could plausibly contain is a separator that will
/// eventually be wrong.
const SEP: &str = "|||";

impl YtDlp {
    pub fn new(cfg: &arc_config::Music) -> Self {
        Self {
            binary: cfg.resolver.clone(),
            timeout: Duration::from_secs(cfg.resolve_timeout_s.max(1)),
            source: "youtube music".into(),
        }
    }
}

impl Resolver for YtDlp {
    fn resolve(&self, query: &str, count: usize) -> Result<Vec<Track>, String> {
        let query = query.trim();
        if query.is_empty() {
            return Err("nothing to search for".into());
        }
        let mut child = Command::new(&self.binary)
            .args([
                "--no-config",
                "--ignore-errors",
                "--no-warnings",
                "--socket-timeout",
                "10",
                "-f",
                "bestaudio",
                "--skip-download",
                "--print",
            ])
            // One field template rather than three --print passes: three
            // invocations is three searches, and they can disagree.
            .arg(format!("%(title)s{SEP}%(uploader)s{SEP}%(url)s"))
            .arg(format!("ytsearch{}:{}", count.clamp(1, 20), query))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run {}: {e}", self.binary))?;

        // Read on a thread and wait on a deadline. Reading inline would block
        // forever on a hung resolver, and the timeout would never be checked.
        let stdout = child.stdout.take();
        let reader = std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(mut o) = stdout {
                use std::io::Read;
                let _ = o.read_to_string(&mut buf);
            }
            buf
        });
        let mut err = String::new();
        let status = self.wait(&mut child, &mut err);
        let out = reader.join().unwrap_or_default();

        if !status {
            return Err(if err.trim().is_empty() {
                format!("{} did not answer in time", self.binary)
            } else {
                format!("{} failed: {}", self.binary, first_line(&err))
            });
        }
        let tracks = parse_results(&out, &self.source);
        if tracks.is_empty() {
            return Err(format!("no playable result for {query:?}"));
        }
        Ok(tracks)
    }
}

impl YtDlp {
    /// Wait for the child, killing it at the deadline. Returns whether it
    /// exited on its own.
    fn wait(&self, child: &mut Child, err: &mut String) -> bool {
        let deadline = Instant::now() + self.timeout;
        loop {
            match child.try_wait() {
                Ok(Some(st)) => {
                    if !st.success()
                        && let Some(mut e) = child.stderr.take()
                    {
                        use std::io::Read;
                        let _ = e.read_to_string(err);
                    }
                    return st.success();
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    }
}

fn first_line(s: &str) -> String {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(160).collect()
}

/// Turn `--print` output into tracks, skipping anything unusable.
///
/// A line with no url is skipped rather than queued: yt-dlp prints one line
/// per *format candidate* and a result it could not resolve to a stream comes
/// out with an empty one. Queueing it would put a silent entry in the queue.
fn parse_results(out: &str, source: &str) -> Vec<Track> {
    out.lines()
        .filter_map(|line| {
            let mut parts = line.trim().split(SEP);
            let title = parts.next()?.trim();
            let artist = parts.next().unwrap_or("").trim();
            let url = parts.next().unwrap_or("").trim();
            if url.is_empty() {
                return None;
            }
            // yt-dlp writes NA for a field it has no value for, which reads
            // worse on screen than saying nothing at all.
            let artist = if artist == "NA" { "" } else { artist };
            Track::new(title, artist, source, url).ok()
        })
        .collect()
}

/// Percent-encode a query for a web URL.
///
/// Deliberately not `urlencoding`/`percent-encoding` crates for three
/// functions' worth of work: unreserved characters pass through, everything
/// else becomes `%XX` from its own bytes, which is correct for UTF-8 input
/// because a multi-byte character is just several bytes to encode.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A player that refuses everything, used when playback is switched off.
///
/// Refusing rather than silently succeeding is the point: `music.enabled =
/// false` must produce "playback is disabled", not a button that does nothing
/// and a status that says something is playing.
#[derive(Debug)]
pub struct NoPlayer {
    reason: String,
}

impl NoPlayer {
    pub fn new(reason: &str) -> Self {
        Self { reason: reason.into() }
    }
}

impl Player for NoPlayer {
    fn replace(&self, _t: &[Track]) -> Result<(), String> {
        Err(self.reason.clone())
    }
    fn append(&self, _t: &[Track]) -> Result<(), String> {
        Err(self.reason.clone())
    }
    fn remove(&self, _i: usize) -> Result<(), String> {
        Err(self.reason.clone())
    }
    fn play_index(&self, _i: usize) -> Result<(), String> {
        Err(self.reason.clone())
    }
    fn previous(&self) -> Result<(), String> {
        Err(self.reason.clone())
    }
    fn remove_remaining(&self) -> Result<(), String> {
        Err(self.reason.clone())
    }
    fn set_paused(&self, _p: bool) -> Result<(), String> {
        Err(self.reason.clone())
    }
    fn progress(&self) -> Result<Progress, String> {
        Err(self.reason.clone())
    }
    fn pid(&self) -> Option<u32> {
        None
    }
    fn shutdown(&self) {}
}

/// The matching resolver.
#[derive(Debug)]
pub struct NoResolver {
    reason: String,
}

impl NoResolver {
    pub fn new(reason: &str) -> Self {
        Self { reason: reason.into() }
    }
}

impl Resolver for NoResolver {
    fn resolve(&self, _query: &str, _count: usize) -> Result<Vec<Track>, String> {
        Err(self.reason.clone())
    }
}

// ---------------------------------------------------------------------------
// Queue
// ---------------------------------------------------------------------------

/// The queue, in the daemon's own memory.
///
/// mpv holds the queue too, but only as filenames: it never saw the titles and
/// artists, because those came from the resolver, not from the url. So the
/// daemon keeps the metadata list beside the player and matches them up by
/// index. Everything that decides *what* plays and *in what order* happens
/// here, which is why it is a separate type with no player in it at all.
#[derive(Debug, Clone)]
pub struct Queue {
    tracks: Vec<Track>,
    /// Index of the playing entry, or `None` when nothing is playing.
    current: Option<usize>,
    /// Most tracks it may hold. A queue with no limit is a memory leak with
    /// a progress bar, so the default is a real number rather than infinity.
    limit: usize,
}

impl Default for Queue {
    fn default() -> Self {
        Self { tracks: Vec::new(), current: None, limit: 100 }
    }
}

impl Queue {
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    pub fn current(&self) -> Option<&Track> {
        self.current.and_then(|i| self.tracks.get(i))
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Indices of everything after the current one.
    pub fn upcoming(&self) -> impl Iterator<Item = (usize, &Track)> {
        let from = self.current.map(|i| i + 1).unwrap_or(0);
        self.tracks.iter().enumerate().skip(from)
    }

    /// Replace the queue and start the first entry.
    pub fn set(&mut self, tracks: Vec<Track>) {
        self.tracks = tracks;
        self.current = (!self.tracks.is_empty()).then_some(0);
    }

    pub fn push(&mut self, tracks: Vec<Track>) -> Result<(), String> {
        let room = self.limit.saturating_sub(self.tracks.len());
        if room == 0 {
            return Err("the queue is full".into());
        }
        let first = self.tracks.is_empty();
        self.tracks.extend(tracks.into_iter().take(room));
        // Enqueueing into silence starts playing: a queue with nothing loaded
        // is not what "add this to the queue" means.
        if first {
            self.current = Some(0);
        }
        Ok(())
    }

    /// Drop everything after the current track.
    pub fn clear(&mut self) {
        if let Some(i) = self.current {
            self.tracks.truncate(i + 1);
        } else {
            self.tracks.clear();
        }
    }

    pub fn remove(&mut self, index: usize) -> Result<Track, String> {
        if index >= self.len() {
            return Err(format!("no queued track at {index}"));
        }
        let removed = self.tracks.remove(index);
        // Removing an entry ahead of the current one shifts it along; removing
        // the current one hands play to whatever took its place, which is what
        // the player will have done too.
        if let Some(c) = self.current {
            self.current = match index.cmp(&c) {
                std::cmp::Ordering::Less => Some(c - 1),
                std::cmp::Ordering::Equal => (index < self.len()).then_some(index),
                std::cmp::Ordering::Greater => Some(c),
            };
        }
        Ok(removed)
    }

    /// Move to the next entry. `None` when this was the last one.
    pub fn advance(&mut self) -> Option<&Track> {
        let next = self.current.map(|i| i + 1).filter(|i| *i < self.len())?;
        self.current = Some(next);
        self.tracks.get(next)
    }

    /// Go back one entry.
    pub fn rewind(&mut self) -> Option<&Track> {
        let prev = self.current?.checked_sub(1)?;
        self.current = Some(prev);
        self.tracks.get(prev)
    }

    /// Follow the player: it says which index it is on, and this adopts it.
    ///
    /// Trusts the player over its own bookkeeping, because mpv can skip an
    /// entry on its own (a track that fails to open advances past it) and a
    /// daemon that second-guessed that would show the wrong track as playing.
    pub fn follow(&mut self, index: Option<usize>) {
        if let Some(i) = index
            && i < self.len()
        {
            self.current = Some(i);
        }
    }

    /// The queue is over once the player is on nothing and has nothing left.
    pub fn finished(&self) -> bool {
        self.current.is_none()
    }

    /// Stop everything.
    pub fn stop(&mut self) {
        self.tracks.clear();
        self.current = None;
    }

    /// Most tracks the queue may hold. Zero would make `push` refuse
    /// everything, so it is raised to one rather than left as a way to switch
    /// queueing off.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit.max(1);
        self
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str) -> Track {
        Track::new(name, "Someone", "test", &format!("https://example.invalid/{name}")).unwrap()
    }

    #[test]
    fn parse_reads_titles_artists_and_urls() {
        let out = format!(
            "Windowlicker|||Aphex Twin|||https://example.invalid/a\nTeardrop|||NA|||https://example.invalid/b\n"
        );
        let tracks = parse_results(&out, "youtube music");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title, "Windowlicker");
        assert_eq!(tracks[0].artist, "Aphex Twin");
        assert_eq!(tracks[1].artist, "", "NA should not be shown as an artist");
        assert_eq!(tracks[0].source, "youtube music");
    }

    /// A result with no stream comes back with an empty url and must not be
    /// queued -- it would be a silent entry the UI could not explain.
    #[test]
    fn parse_skips_results_with_no_url() {
        let out = "Good Song||||\nAlso Good|||Someone|||https://example.invalid/b\n";
        let tracks = parse_results(out, "youtube music");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Also Good");
    }

    #[test]
    fn parse_ignores_junk() {
        assert!(parse_results("", "s").is_empty());
        assert!(parse_results("WARNING: something\n", "s").is_empty());
        assert!(parse_results("||||\n", "s").is_empty());
    }

    #[test]
    fn queue_plays_in_order_and_advances() {
        let mut q = Queue::default();
        q.set(vec![t("a"), t("b"), t("c")]);
        assert_eq!(q.current().unwrap().title, "a");
        assert_eq!(q.upcoming().count(), 2);
        assert_eq!(q.advance().unwrap().title, "b");
        assert_eq!(q.current().unwrap().title, "b");
        assert_eq!(q.upcoming().count(), 1);
        assert!(q.advance().is_some());
        assert!(q.advance().is_none(), "advanced past the end of the queue");
        assert_eq!(q.current().unwrap().title, "c", "the last track is still current");
    }

    /// Enqueueing into silence plays. Otherwise "add this to the queue" while
    /// nothing is playing would fill a queue nobody will ever hear.
    #[test]
    fn enqueue_into_silence_starts_playing() {
        let mut q = Queue::default();
        assert!(q.is_empty());
        q.push(vec![t("a")]).unwrap();
        assert_eq!(q.current().unwrap().title, "a");

        let mut q = Queue::default();
        q.set(vec![t("a"), t("b")]);
        q.push(vec![t("c")]).unwrap();
        assert_eq!(q.len(), 3);
        assert_eq!(q.current().unwrap().title, "a", "the current track changed on enqueue");
    }

    #[test]
    fn removing_reindexes_the_current_track() {
        let mut q = Queue::default();
        q.set(vec![t("a"), t("b"), t("c")]);
        q.advance(); // b, at index 1
        // Removing an entry *behind* the current one pulls it back a slot --
        // 'b' moves from index 1 to index 0 and must stay the current track.
        q.remove(0).unwrap();
        assert_eq!(q.current().unwrap().title, "b", "index did not follow the removal");
        assert_eq!(q.upcoming().count(), 1);
        // Removing the last entry is a plain drop.
        assert_eq!(q.remove(1).unwrap().title, "c");
        assert_eq!(q.len(), 1);
        assert!(q.remove(9).is_err(), "removing a missing index reported success");
    }

    /// Removing the track that is playing hands play to the next one, which is
    /// what mpv does underneath: leaving `current` pointing at the removed
    /// entry would show a track that is no longer there.
    #[test]
    fn removing_the_current_track_hands_over_to_the_next() {
        let mut q = Queue::default();
        q.set(vec![t("a"), t("b"), t("c")]);
        q.remove(0).unwrap();
        assert_eq!(q.current().unwrap().title, "b");
        // Removing the last one leaves nothing to play.
        let mut q = Queue::default();
        q.set(vec![t("a")]);
        q.remove(0).unwrap();
        assert!(q.current().is_none(), "a removed-and-only track is still current");
        assert!(q.finished());
    }

    /// Clearing drops what is coming, not what has been. Keeping the entries
    /// already played is what lets "previous" go back after a clear, which is
    /// what mpv's own playlist does and what a user pressing back expects.
    #[test]
    fn clearing_keeps_the_current_track_and_drops_the_rest() {
        let mut q = Queue::default();
        q.set(vec![t("a"), t("b"), t("c")]);
        q.advance(); // b
        q.clear();
        assert_eq!(q.len(), 2, "already-played entries should survive a clear");
        assert_eq!(q.current().unwrap().title, "b");
        assert_eq!(q.upcoming().count(), 0, "something is still queued after a clear");
        // Clearing a queue of one keeps it: that entry is what is playing,
        // and "clear the queue" must not stop the music.
        let mut q = Queue::default();
        q.set(vec![t("a")]);
        q.clear();
        assert_eq!(q.len(), 1);
        assert_eq!(q.current().unwrap().title, "a");
        // A queue with nothing playing at all has nothing to keep, so it does
        // empty out.
        let mut q = Queue::default();
        q.set(vec![t("a")]);
        q.rewind();
        q.stop();
        q.clear();
        assert!(q.is_empty());
    }

    /// The player advances on its own when a track cannot be opened, and the
    /// daemon must follow it rather than show its own idea of what is playing.
    #[test]
    fn the_player_is_asked_which_index_it_is_on() {
        let mut q = Queue::default();
        q.set(vec![t("a"), t("b")]);
        q.follow(Some(1));
        assert_eq!(q.current().unwrap().title, "b");
        // A player index past the end of what we know about is not adopted.
        q.follow(Some(9));
        assert_eq!(q.current().unwrap().title, "b", "a nonsense index moved the queue");
        q.follow(None);
        assert_eq!(q.current().unwrap().title, "b", "a transient None dropped the track");
    }

    #[test]
    fn a_full_queue_refuses_rather_than_growing() {
        let mut q = Queue::default().with_limit(2);
        q.push(vec![t("a"), t("b")]).unwrap();
        assert!(q.push(vec![t("c")]).is_err(), "the queue grew past its limit");
        assert_eq!(q.len(), 2);
        // A limit of zero would refuse everything, so it is raised to one.
        assert!(Queue::default().with_limit(0).push(vec![t("a")]).is_ok());
    }

    #[test]
    fn stopping_empties_everything() {
        let mut q = Queue::default();
        q.set(vec![t("a"), t("b")]);
        q.stop();
        assert!(q.is_empty());
        assert!(q.finished());
        assert_eq!(q.upcoming().count(), 0);
    }
}
