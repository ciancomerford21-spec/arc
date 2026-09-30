# Music: the daemon owns the player

**The bug this replaced.** `play_youtube_music` resolved a search with `yt-dlp`
and started `mpv` in the background, then exited. Nothing Arc started was still
running by the time anyone wanted to pause it, and a process launched by a script
that has exited has no handle to talk to — no pid the daemon knows, no socket,
nothing. Every control question ("pause", "skip", "what's next") had the same
answer: impossible. The status line in the overlay reported a track it could not
act on.

## Decisions this left open

* **The daemon owns mpv, not a detached process.** `MpvPlayer` holds the child
  and talks to it over mpv's JSON IPC on a Unix socket. Detaching was the
  original mistake and is not recoverable after the fact, so nothing here
  detaches.
* **mpv's queue mirrors the daemon's; mpv's index is the truth.** The daemon
  keeps the queue for metadata and history, mpv answers what is actually
  playing and how far through it is. A 1 Hz supervisor reconciles the two, which
  is also how the daemon notices a track ending without being asked.
* **A resolved stream URL never leaves the daemon.** `Track::url` is
  `#[serde(skip_serializing)]`. YouTube URLs expire in hours, so a client
  holding one has a stale string; nothing outside needs it anyway.
* **Position is polled, not evented.** `MusicStatus` carries the track, the
  queue and the state. It does *not* carry the playhead: an event per second
  would repaint every client for a progress bar. `arc music position` exists for
  that, and the overlay polls it once a second only while the section is open.
* **`paused` is distinct from `stopped`.** A paused track keeps its title and
  its place; a stopped one is gone. Collapsing them is why a player UI feels
  like it lost its state when you reach for the space bar.
* **`previous` delegates to `playlist-prev`**, which restarts the current track
  when it is more than a few seconds in. That is what every other player does,
  and it is not what "index minus one" does.
* **`clear` empties the queue but keeps playing; `stop` resets everything.**
  They are different intentions, and conflating them loses a queue.
* **A browser fallback says so and claims nothing.** If nothing resolves, the
  search page opens and the reply is a message. No title goes on screen, because
  no track is playing.

## The mpv you actually have

This build is mpv 0.41 and **has no `playlist-append` and no `playlist-count`** —
both return `invalid parameter`. The manual is wrong; `mpv --input-cmdlist` is
the authority. Every command used here was verified against the binary on this
machine before any code was written:

| Need | Command |
|---|---|
| replace / add to the queue | `loadfile <url> replace\|append` |
| what is playing | `playlist`, `playlist-pos` |
| playhead | `time-pos`, `duration`, `idle-active` |
| pause | `set_property pause <bool>` |
| skip | `playlist-play-index`, `playlist-next`, `playlist-prev` |
| edit | `playlist-remove`, `playlist-move`, `playlist-clear` |

If you extend this, verify against the binary first. The e2e tests run the real
player code against a stub that speaks this protocol and rejects anything else,
so a wrong command name fails the suite rather than failing silently on a
user's machine.

## What was built

| Piece | File |
|---|---|
| `Track`, `MusicStatus`, `MusicRequest`, `Request::Music`, `Event::Music` | `crates/arc-proto/src/lib.rs` |
| `[music]` config: enabled, player, resolver, queue_limit, fallback | `crates/arc-config/src/lib.rs`, `config/config.toml` |
| `Player`/`Resolver` traits, `MpvPlayer`, `YtDlp`, the bounded `Queue` | `crates/arc-daemon/src/music.rs` |
| Queue state, every action, status/event publication | `crates/arc-daemon/src/state.rs` |
| The 1 Hz supervisor and socket routing | `crates/arc-daemon/src/lib.rs`, `src/server.rs` |
| `arc music show\|position\|play\|enqueue\|pause\|resume\|toggle\|next\|previous\|stop\|clear\|remove` | `crates/arc-cli/src/main.rs` |
| `media_play`/`media_pause`/`media_next` prefer the daemon, MPRIS as fallback | `crates/arc-tools/src/lib.rs` |
| Spoken phrasings of every transport command | `crates/arc-core/src/lib.rs` |
| The playback script, now a thin adapter | `integrations/tools/play_youtube_music/` |
| The MUSIC section: progress, transport, queue, add-to-queue | `integrations/arc-app/shell.qml` |

## The overlay's MUSIC section

A section of its own, not more header: pause and skip are controls with state,
and a header chip is the wrong shape for a queue you add to and remove from.

* Collapsed: a single line — an add-to-queue field and a disclosure.
* Expanded: current track, progress bar, transport, the queue, and an add field.
* The playhead poll runs only while it is open and something is playing. A
  widget nobody is looking at should not spawn a process every second.
* A refusal is shown next to the buttons. `next` with nothing queued does
  nothing, and a button that silently does nothing is indistinguishable from a
  broken one.

`musicNow` and `musicQueue` are guarded accessors rather than `music.now` and
`music.queue` read directly: those bindings also evaluate while nothing is
playing, and a binding that throws on null is re-evaluated forever.

## Voice

`play_youtube_music` calls `arc --json music play` and speaks the title the
daemon resolved — never the query. `media_pause`, `media_play` and `media_next`
go to the daemon's own player when there is one and fall back to MPRIS for
external players.

Router rules match the phrasings people actually use: "pause", "pause the
music", "can you pause the song", "pause playback", "skip to the next track",
"go back", "resume". Anchored to the bare verb, every one of those except
`pause` reached the model — which would then answer by talking about music
rather than pausing it. The rules are asserted with `route == FixedCommand`, not
just the tool name, because the model can pick the same tool on the fallback
path. A sentence that merely *mentions* music ("how do i pause playback in
mpv") must still reach the model.

## Tests

```sh
cargo test --workspace                # 385 tests
scripts/test-music-cli.sh             # real `arc` against a real `arcd`
scripts/test-music-tool.sh            # the script's contract, exit codes, refusals
scripts/check-qml.sh                  # shell.qml loads in the real engine
```

The CLI test drives a real daemon against a stub player and resolver that speak
mpv's actual IPC, so the wire protocol is covered — and the QML check runs
`quickshell -p`, which is the only thing that catches a binding throwing on null
or a component that does not compile. Both stubs mean no audio is played and no
network is touched.

## Deploying

The daemon loads tools from `~/.local/share/arc/`, not the working tree, and
`~/.config/quickshell/arc/` is what Quickshell runs. A correct source file that
was never installed ships nothing:

```sh
cargo build --release
install -Dm755 target/release/arcd ~/.local/bin/arcd
install -Dm755 target/release/arc  ~/.local/bin/arc
scripts/install-tools.sh              # syncs integrations/tools → ~/.local/share/arc
install -Dm644 integrations/arc-app/shell.qml ~/.config/quickshell/arc/shell.qml
systemctl --user restart arcd
```

The live `~/.config/arc/config.toml` needs a `[music]` section; without it the
daemon warns and serves the rest. The shipped values in `config/config.toml` are
the defaults.