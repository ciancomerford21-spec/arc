#!/usr/bin/env bash
# Confirm the installed playback tool is one the daemon will actually load:
# the script passes the shell screen and its recorded fingerprint matches the
# text on disk. A tool that fails either is skipped at start with a log line
# and simply does not exist, which is otherwise invisible.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLS="${ARC_TOOLS_DIR:-$HOME/.local/share/arc/tools}"
export ARC_SOCKET="$ROOT/target/debug/.verify.sock"

work="$(mktemp -d)"
mkdir -p "$work/config" "$work/run"
printf '[ai]\nprovider = "none"\n' >"$work/config/config.toml"
ARC_SOCKET="$work/run/arc.sock" \
ARC_RUNTIME_DIR="$work/run" \
ARC_CONFIG_DIR="$work/nope" \
ARC_MEMORY_PATH="$work/mem" \
ARC_TOOLS_DIR="$TOOLS" \
ARC_TOOL_CLASSES="$work/classes.json" \
ARC_AUTOMATIONS="$work/automations.toml" \
ARC_LOG=info \
  "$ROOT/target/debug/arcd" --config "$work/config/config.toml" --no-voice --no-bar >"$work/log" 2>&1 &
pid=$!
trap 'kill "$pid" 2>/dev/null; rm -rf "$work"' EXIT
for _ in $(seq 1 100); do [ -S "$work/run/arc.sock" ] && break; sleep 0.1; done
sleep 1

fail=0
if grep -qi "play_youtube_music" "$work/log"; then
  echo "FAIL: the daemon complained about the tool:"; grep -i "play_youtube_music" "$work/log"
  fail=1
else
  echo "ok: the daemon logged no complaint about play_youtube_music"
fi
if grep -q "now-playing" "$TOOLS/play_youtube_music/run.sh" 2>/dev/null; then
  echo "ok: the installed script reports the track"
else
  echo "FAIL: the installed script does not report the track"; fail=1
fi
exit "$fail"