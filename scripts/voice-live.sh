#!/usr/bin/env bash
# Interactive end-to-end voice test on the real mic + speaker.
# Runs arcd on a private socket in wake-word mode, with no language model.
# Say, for example:  "Hey Arc, battery status"  or  "Hey Arc, set volume to 30"
# Ctrl-C to stop.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
run="$(mktemp -d)"
cfg="$run/config.toml"
printf '[ai]\nprovider = "none"\n[voice]\nmode = "wake_word"\n' >"$cfg"
export ARC_SOCKET="$run/arc.sock" ARC_RUNTIME_DIR="$run"

"$root/target/debug/arcd" --config "$cfg" 2>"$run/arcd.log" &
pid=$!
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null; rm -rf "$run"' EXIT
for _ in $(seq 60); do [[ -S $ARC_SOCKET ]] && break; sleep 0.25; done
sleep 3
echo 'Listening. Say "Hey Arc, battery status". Ctrl-C to stop.'
printf '{"id":1,"type":"subscribe","topics":["assistant","errors"]}\n' |
    socat -t 100000 - "UNIX-CONNECT:$ARC_SOCKET" |
    python3 -u "$root/scripts/print-events.py"
