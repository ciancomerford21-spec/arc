# Arc

> **Note on this change (per-tool safety classification).** The Arc app's
> TOOLS pane now has a three-cell selector on every tool row — `safe`,
> `caution`, `dangerous` — driven by `RiskPicker` in
> `integrations/arc-app/shell.qml`. Choices made there, and the reasonable
> defaults the task left open:
>
> * **Storage.** `~/.config/arc/tool_classes.json` (override:
>   `$ARC_TOOL_CLASSES`), not a `[tools]` table in `config.toml`. The UI
>   rewrites this on every click; rewriting a file the user may be editing in
>   a text editor is how comments get lost. The daemon is the only writer and
>   loads the file once at start, so a classification survives a restart.
> * **Precedence.** Raising is unconditional: `dangerous` always prompts, even
>   where a policy would have allowed the call. Lowering only changes a tool's
>   *baseline*. Any call the tool itself judges dangerous stays dangerous, so
>   marking `shell_exec` safe quiets `ls` but `rm -rf ~/x` or
>   `systemctl poweroff` still ask. Tools that are dangerous by nature
>   (`reboot`, `shutdown`, `code`) cannot be lowered at all. A classification is
>   not a permission: blocked calls stay refused, and a `deny` rule or a
>   disabled tool still wins.
> * **Reset.** Clicking the already-active cell, or the `↺` button that appears
>   only while overridden, clears the override and restores the built-in level.
> * **CLI equivalent.** `arc tool <name> {safe|caution|dangerous|default|show}`,
>   which is what the app shells out to.

A local-first voice assistant for Omarchy (Hyprland). Say **"Hey Arc, …"** and it
controls your desktop, apps, audio, media and system, or just talks.

Nothing leaves the machine. The AI, the speech recognition and the speech
synthesis are all local by default, and the default configuration has no cloud
fallback and no cloud phrasing.

## Install

```bash
scripts/fetch-models.sh        # voice models (VAD, speech-to-text, text-to-speech), once
scripts/install.sh             # build, install, start services, add the bar widget
```

Everything is per-user; no root. `scripts/uninstall.sh` removes it again
(`--purge` also deletes config, secrets and models).

| What | Where |
|---|---|
| Binaries | `~/.local/bin/arc`, `~/.local/bin/arcd` |
| Config | `~/.config/arc/config.toml` |
| API keys | `~/.config/arc/secrets.env` (mode 600, `NAME=value` lines) |
| Voice venv and speech models | `~/.local/share/arc/` |
| Memory store | `~/.local/share/arc/memory/` |
| Services | `arcd.service`, `hermes-proxy.service` (systemd user units) |
| Bar widget | `~/.config/omarchy/plugins/arc.status` |

## Using it

- **Voice:** "Hey Arc, switch to workspace 3", "Hey Arc, volume 30".
- **Bar icon:** left-click opens the panel (live transcript, pending
  confirmation), right-click starts push-to-talk, middle-click stops speaking.
- **Panel keys:** `y` / `n` answer a confirmation, `c` clears the transcript,
  `Esc` closes, `Tab` hands the keyboard back to your window.
- **Terminal:** `arc ask "open firefox"`, `arc status`, `arc watch`, `arc tools`.
- **Risky actions** (power, deleting files, shell commands outside the allow-list) ask first:
  answer "yes"/"no" out loud, or `arc confirm` / `arc reject`.
- **Follow-ups:** when Arc asks you something (a confirmation or a question), it listens for
  about 5 seconds afterwards; just answer, no "Hey Arc" needed. Turn off with `follow_up = false`
  under `[voice]`.

## Custom commands

Add your own phrases in `~/.config/arc/automations.toml`, then `systemctl --user restart arcd`:

```toml
[[automation]]
name = "coding mode"                      # saying the name runs it
triggers = ["start coding", "dev mode"]   # other phrases that run it
steps = [
  { tool = "app_launch", args = { app = "code", workspace = 2 } },
  { wait_ms = 700 },
  { tool = "open_url", args = { url = "github.com", workspace = 3 } },
  { say = "Coding mode is ready." },      # what Arc says at the end
]
```

- Phrases match exactly, ignoring case, punctuation, "please" and "Hey Arc". Anything else goes to the AI as usual.
- `arc tools` lists the tools; `config/automations.toml` in this repo documents their arguments.
- Steps go through the normal permission checks. `continue_on_error = true` keeps going after a failed step.
- Mistakes (unknown tool, bad TOML) show up in `journalctl --user -u arcd`; Arc still starts.

## Memory

Arc keeps two things on disk in `~/.local/share/arc/memory/`, and both survive
restarts:

- `facts.json` — things worth keeping: preferences, names, your setup.
- `sessions.json` — transcripts of recent conversations.

On every reply Arc prepends a `MEMORIZED FACTS` and `RECENT CONVERSATION`
block to the system prompt, so it can answer "what did I just tell you?" and
"what editor do I use?" in a session where you never said them. Recalled
content is labelled as unverified history, so Arc does not treat an old
assistant claim as a current fact. The transcript block is capped at the last
24 turns (`MAX_TURNS_IN_PROMPT`), so the prompt stays bounded.

You rarely need the CLI, but it's there:

```sh
arc memory list                          # everything remembered, with ids
arc memory search tea                    # filter by keyword
arc memory remember editor = "neovim"    # store a fact by hand
arc memory forget <id-or-key>            # remove one
arc memory forget-last                   # undo the most recent
arc memory clear                         # remove everything
```

Arc also has four tools it can call itself — `memory_remember`,
`memory_forget`, `memory_list`, `memory_search` — so you can just say
"remember that I drink tea" and it will. Set `ARC_MEMORY_PATH` to move the
store somewhere else.

## Language model

Cloud by default, and the only option. Arc talks to the Hermes proxy
(`hermes-proxy.service`, an OpenAI-compatible endpoint on
`http://127.0.0.1:8645/v1`), which attaches your own Hermes credentials. The
proxy is Arc's only reasoning provider, so it is enabled at boot; if it is
down, Arc cannot answer.

There is no local model. The 2B that used to run here refused coherent tasks
("I can't create applications") and answered flatly, and it cost ~3.6GB of
VRAM. Voice stays local: Moonshine for speech recognition and Kokoro for
speech, both under `[voice]`.

Other providers remain implemented — set `provider` in `[ai]` to:

- `hermes`: the proxy above.
- `openai`: any OpenAI-compatible API. `[ai.openai]` holds the endpoint and
  model; the key is read from the variable named in `api_key_env`.
- `anthropic`: the Messages API.
- `none` (built-in commands only).

`fallback` is `none`: there is no local model left to degrade to, so a proxy
outage means no reply rather than a worse one. Requests are retried once on a
transient 5xx, which the proxy throws on roughly 2 in 5 calls; a 401 or 400
fails immediately, because retrying those just wastes time.

`phrasing` is set to `hermes`, the same connection as the primary, so replies
are not rewritten on a second connection. Pointing it at another provider
costs an extra round trip per reply for no measured gain.

### What the model decides, and what it does not

- **Known commands bypass the model.** "open wikipedia.org" opens the site
  directly. A dot or a TLD is required, so "open spotify" is still
  treated as an app request. Anything that asks for an answer ("search X and tell me about it")
  goes to the model instead, because there is no fetch tool to satisfy it.
- **The prompt carries a personality.** `[personality] custom_prompt` sets the
  voice; it ships filled in and lives in `Config::default()`, so a fresh
  install sounds the same. It used to be forced short because a long prompt
  stopped the 2B calling tools — that constraint is gone, and re-measured the
  full prompt costs nothing against a cloud model.

### The `code` tool

`[code] enabled = true` hands multi-step tasks to `hermes chat -q`, which
acts on the machine itself: build a project, debug a failing build,
investigate a codebase. It writes files and runs commands, so it is always
`Dangerous` and always confirms. The task is passed via `--query-file` so the
shell cannot reinterpret what the model wrote, and the task text is screened
for destructive commands. `workspace` bounds where it may write.


Under `[voice]`, all local:

| Setting | Default |
|---|---|
| `stt_engine` / `stt_model` | `moonshine`, `sherpa-onnx-moonshine-base-en-quantized-2026-02-27` |
| `tts_engine` | `kokoro` (also `piper`, `espeak`, `none`) |
| `tts_voice` | `kokoro-en-v0_19` — a model *directory*, not an `.onnx` filename |
| `tts_speaker` | `af_bella` |

## Troubleshooting

```bash
systemctl --user status arcd hermes-proxy
journalctl --user -u arcd -f          # daemon + voice log
arc status                            # component health
ARC_VOICE_DEBUG=1 arcd                # log every transcript the mic hears (privacy: includes nearby talk)
```

`arc status` reports `voice.tts` as `ok` once the model *loads*. It does not
play a test clip, so it will happily say `ok` while nothing is audible. If Arc
is silent, check for a playback process directly:

```bash
ps -eo args | grep "pw-cat --playback"   # present only while Arc is speaking
```

If that never appears, the running sidecar is executing stale Python. Restart
it — `scripts/deploy-voice.sh` overwrites the package in
`~/.local/share/arc/python/` and Python does not reload modules in a live
process:

```bash
systemctl --user restart arcd
```

If the wake word isn't picked up, run with `ARC_VOICE_DEBUG=1` and look at how
the recogniser spells "Arc"; add that spelling to `name_variants` under `[general]`.

## Omarchy integration

The bar widget is a normal third-party Omarchy plugin, so it can be removed
without touching anything else:

```bash
rm -rf ~/.config/omarchy/plugins/arc.status
omarchy-shell shell rescanPlugins
```

Its source is `integrations/omarchy-bar/arc.status/`, and `install.sh` copies
it into place. Arc's services stay systemd units: the shell can crash and
restart without taking your voice assistant down, and the assistant keeps
talking when the shell is gone.

`integrations/waybar/` holds an older Waybar status bar for people not running
Omarchy. It is not installed by `scripts/install.sh`.

## Development

```bash
cargo test                             # 198 tests
~/.local/share/arc/venv/bin/python -m pytest python/tests   # 67 tests
scripts/voice-live.sh                  # live mic test on a private socket
scripts/daemon-smoke.sh                # exercise the daemon end to end
```

The workspace is eleven crates: `arc-proto`, `arc-config`, `arc-security`,
`arc-hyprland`, `arc-system`, `arc-tools`, `arc-ai`, `arc-core`, `arc-memory`,
`arc-daemon`, `arc-cli`.

A test asserts that the shipped `config/config.toml` matches the code defaults
and that it contains no key the code does not recognise, so a setting that
stops being read fails the build rather than sitting there quietly.
