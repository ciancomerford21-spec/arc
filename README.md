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
