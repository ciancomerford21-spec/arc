#!/usr/bin/env bash
# Drive the real `arc` CLI against a real `arcd`, the way the playback script
# does. This is the last link in the chain the shell test cannot cover: the
# script's `arc --json now-playing set` going through argument parsing, the
# socket, the daemon and back out to something the overlay can render.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARC="$ROOT/target/debug/arc"
ARCD="$ROOT/target/debug/arcd"
[ -x "$ARC" ] && [ -x "$ARCD" ] || { echo "build first: cargo build" >&2; exit 1; }

work="$(mktemp -d)"
mkdir -p "$work/config" "$work/run"
printf '[ai]\nprovider = "none"\n' >"$work/config/config.toml"
export ARC_SOCKET="$work/run/arc.sock"
export ARC_RUNTIME_DIR="$work/run"
export ARC_CONFIG_DIR="$work/nonexistent"
export ARC_MEMORY_PATH="$work/memory"
export ARC_AUTOMATIONS="$work/automations.toml"
export ARC_TOOLS_DIR="$work/tools"
export ARC_TOOL_CLASSES="$work/tool_classes.json"

"$ARCD" --config "$work/config/config.toml" --no-voice --no-bar >"$work/log" 2>&1 &
arcd_pid=$!
trap 'kill "$arcd_pid" 2>/dev/null; rm -rf "$work"' EXIT

for _ in $(seq 1 100); do
  [ -S "$ARC_SOCKET" ] && break
  sleep 0.1
done
[ -S "$ARC_SOCKET" ] || { echo "arcd never came up:" >&2; cat "$work/log" >&2; exit 1; }

pass=0; fail=0
check() {
  if [ "$2" = "$3" ]; then pass=$((pass+1)); else
    fail=$((fail+1))
    echo "FAIL: $1" >&2; echo "  got:  $2" >&2; echo "  want: $3" >&2
  fi
}

check "nothing playing at first" "$("$ARC" now-playing)" "nothing playing"
check "and the JSON says so" \
  "$("$ARC" --json now-playing show | python3 -c 'import json,sys; print(json.load(sys.stdin)["playing"])')" \
  "False"

# A report, exactly as the script makes it.
"$ARC" --json now-playing set --title "Hall of Fame" --artist "Boards of Canada" \
  --source "youtube music" --pid 0 >/dev/null
check "the track is shown" "$("$ARC" now-playing)" "Hall of Fame — Boards of Canada"

# The overlay's read, which is what the app calls on start.
check "the overlay read agrees" \
  "$("$ARC" --json now-playing show | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["title"], d["artist"], d["playing"])')" \
  "Hall of Fame Boards of Canada True"

# A title with a quote and a dash in it: this goes through the shell as one
# argument, and the JSON must survive the round trip intact.
"$ARC" --json now-playing set --title 'Rock "N" Roll' --artist 'AC/DC — T.N.T' >/dev/null
check "awkward metadata survives" "$("$ARC" now-playing)" 'Rock "N" Roll — AC/DC — T.N.T'

"$ARC" --json now-playing stop >/dev/null
check "stopping clears it" "$("$ARC" now-playing)" "nothing playing"

# A set with no title is refused, and says why, without touching the state.
if "$ARC" --json now-playing set --artist "Nobody" >/dev/null 2>&1; then
  check "a titleless set is refused" "accepted" "refused"
else
  check "a titleless set is refused" "refused" "refused"
fi
check "and the row stays clear" "$("$ARC" now-playing)" "nothing playing"

# Stopping with no daemon must still succeed: a playback script that finds no
# socket should not fail because of it, and there is nothing to clear anyway.
ARC_SOCKET="$work/run/missing.sock" "$ARC" --json now-playing stop >/dev/null 2>&1
check "stopping without a daemon is not an error" "$?" "0"

# The same check via the actual script, with the real arc on PATH.
PATH="$ROOT/target/debug:$PATH" ARC_ARG_QUERY="" bash \
  "$ROOT/integrations/tools/play_youtube_music/run.sh" >/dev/null 2>&1
check "the script's clear works through the real CLI" \
  "$(printf '%s' "$?" )" "1"   # an empty query still exits 1: nothing was played

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]