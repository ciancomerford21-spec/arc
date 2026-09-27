# Arc

A local-first voice assistant for Omarchy (Hyprland). Say **"Hey Arc, …"** and it
controls your desktop, apps, audio, media and system, or just talks.

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
| Voice venv, models, llama.cpp | `~/.local/share/arc/` |
| Services | `arcd.service`, `arc-llm.service` (systemd user units) |
| Bar widget | `~/.config/omarchy/plugins/arc.status` |

## Using it

- **Voice:** "Hey Arc, what time is it?", "Hey Arc, switch to workspace 3", "Okay Arc, volume 30".
- **Bar icon:** click to talk, right-click for a live activity log, middle-click to stop speaking.
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
"what editor do I use?" in a session where you never said them. The transcript
block is capped at the last 24 turns, so the prompt stays bounded.

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

Set in `[ai]` of the config: `provider` is tried first, `fallback` if it fails.

- `openai`: any OpenAI-compatible API. Configured for Groq by default via `[ai.openai]`;
  the key is read from `GROQ_API_KEY` in the environment or `secrets.env`.
- `local`: llama.cpp on this machine (`arc-llm.service`, model in `[ai.local]`).
  Nothing leaves the PC. The installer only enables the service if the config uses it.
- `anthropic`, `none` (built-in commands only).

## Troubleshooting

```bash
systemctl --user status arcd arc-llm
journalctl --user -u arcd -f          # daemon + voice log
arc status                            # component health
ARC_VOICE_DEBUG=1 arcd                # log every transcript the mic hears (privacy: includes nearby talk)
```

If the wake word isn't picked up, run with `ARC_VOICE_DEBUG=1` and look at how
the recogniser spells "Arc"; add that spelling to `name_variants` under `[general]`.

## Development

```bash
cargo test -- --test-threads=1
~/.local/share/arc/venv/bin/python -m pytest python/tests
scripts/voice-live.sh                 # live mic test on a private socket
```
