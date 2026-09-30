# Now playing: showing the current track in the Arc overlay

**Decisions this task left open, made here and written down.**

* **Where the status lives.** In the daemon, not in a file. `~/.local/share/arc/`
  is the tools directory and the daemon already owns every piece of mutable
  state; adding a second source of truth for "what is playing" would be one more
  thing to keep in sync. The status is reachable over the existing socket, which
  means it inherits the daemon's UID check and needs no new IPC surface.
* **The overlay is `integrations/arc-app/shell.qml`** (the standalone Quickshell
  app), not the Omarchy bar widget. That is the one with a header and room for
  a track line. The bar widget is fed from the same stream and gets the track
  in its tooltip for free, because the daemon puts it there.
* **Clearing on stop is the daemon's job, driven by the player pid.** The tool
  that starts a track exits immediately, so nothing can tell anyone when the
  track ends. The pid is the only signal that exists. A report with `pid = 0`
  (nobody said) is never reaped -- reaping on it would clear the row for any
  report that omits the pid, which is the common case for a browser fallback.
* **The browser fallback clears the row rather than setting it.** Nothing Arc
  started is playing at that point, so putting a title on screen would be a
  claim it cannot support. The search page opens; the overlay says nothing.
* **The track is shown from yt-dlp metadata, not from the query.** "play
  hall of fame" should put "Hall of Fame — Boards of Canada" on screen, not the
  user's own words back at them. The query is the fallback when yt-dlp gives no
  title.
* **Titles are capped at 200 characters.** This is scraped web metadata going
  onto one line of a header chip.
* **The `✕` in the overlay clears the row; it does not kill the player.**
  Killing another process on a UI click is not something to do by accident.

## What was built

| Piece | File |
|---|---|
| `NowPlaying` type, `NowPlayingRequest`, `now_playing` event, `now_playing` field on the bar and status reports | `crates/arc-proto/src/lib.rs` |
| Storage, set/stop/show, pid reaping | `crates/arc-daemon/src/state.rs` |
| Socket request handling | `crates/arc-daemon/src/server.rs` |
| The reap timer | `crates/arc-daemon/src/lib.rs` |
| `arc now-playing show\|set\|stop` | `crates/arc-cli/src/main.rs` |
| The track in the overlay header | `integrations/arc-app/shell.qml` |
| The playback tool, reporting itself | `integrations/tools/play_youtube_music/run.sh` |

## How the pieces talk

```
play_youtube_music/run.sh
  mpv starts, title+artist from yt-dlp
      └─> arc --json now-playing set --title … --artist … --pid $MPV_PID
              └─> daemon stores it, emits Event::NowPlaying (topic: assistant)
                      ├─> overlay's nowPlaying, and the bar's now_playing
                      └─> overlay's ✕ / a stop → clears
  mpv exits
      └─> daemon's 1s poll sees the pid is gone → emits the same stop
```

The reap is why the row disappears when a track ends rather than at the next
request.

## Installing the tool

The live copy of a self-made tool lives in `~/.local/share/arc/tools/`, outside
this repository. The repo copy under `integrations/tools/` is the reviewable
source of truth; sync it with:

```bash
scripts/install-tools.sh          # integrations/tools/ -> ~/.local/share/arc/tools/
```

It recomputes the script fingerprint rather than carrying it, because the
daemon compares the fingerprint against the script text to decide whether a
script was edited after you reviewed it — a stale one would keep a
classification you gave for text you never read.

Restart `arcd` after syncing (tools are loaded once at start).

## Tests

```bash
cargo test --workspace            # 355 tests, incl. the new now-playing ones
scripts/test-music-tool.sh        # the script's reporting, with stub players
scripts/test-now-playing-cli.sh   # the real CLI against a real arcd
scripts/check-qml.sh              # the overlay loads in the real Quickshell
```

What is covered, and what each check is for:

* **`arc-proto`** (unit) — `label`/`playing` are derived on the daemon so the
  overlay does not reimplement them; a stopped report with a stale title still
  serializes as not playing; an old writer that sends only `title` still
  decodes; unknown fields do not break a reader; `now_playing` is *absent* from
  an idle bar rather than present and empty, so an old widget ignores it.
* **`arc-daemon`** (unit) — a report is announced and a stop clears it; repeat
  reports emit nothing (otherwise the row flickers); `show` neither emits nor
  disturbs state; a blank title is refused; a live pid is not reaped, pid 0 is
  not reaped, a dead one is; the bar and status carry the track only while it
  plays; absurd metadata is truncated.
* **`arc-daemon`** (end to end, real `arcd` over a real socket) — the full
  report → event → read → stop round trip, and the reap firing on its own from
  a dead pid with no further requests.
* **`scripts/test-music-tool.sh`** — the real script with stub `yt-dlp`, `mpv`
  and `arc` on PATH: the success path reports mpv's *own* pid (not the
  script's), a `yt-dlp` that finds nothing clears instead of claiming a track,
  and the browser fallback never claims one. The no-player case runs with a
  PATH that provably has no mpv and no yt-dlp — `/usr/bin` cannot be on it,
  because the real ones are there and the test would silently check the wrong
  path.
* **`scripts/test-now-playing-cli.sh`** — the actual CLI against an actual
  daemon: argument parsing, the socket, and metadata containing quotes and an
  em dash surviving the round trip intact. Also that `now-playing stop`
  succeeds with no daemon, because a playback script that finds no socket must
  not fail because of it.
* **`scripts/check-qml.sh`** — loads the overlay in the real Quickshell engine.
  A hand-written brace checker was tried first and thrown out: QML embeds
  JavaScript, whose regex literals contain braces a lexer cannot tell from
  code, and it reported the unmodified file as broken.

## One bug worth naming

`process_alive` originally cast the pid `as i32`. `u32::MAX as i32` is `-1`, and
`kill(-1, 0)` means *every process you may signal* — it returns success for a
pid that has never existed, so the row would have been stuck on screen forever.
It now converts through `pid_t` and refuses anything that would wrap. The
regression test is the `u32::MAX` end-to-end case: it is the only way to reach
that value from a real report.