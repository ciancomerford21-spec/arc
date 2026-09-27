#!/usr/bin/env bash
# Live smoke test of the voice service against the real microphone.
# Starts a capture, records for N seconds, stops, and prints the reports.
#   scripts/voice-smoke.sh [seconds]
set -euo pipefail
secs="${1:-4}"
root="$(cd "$(dirname "$0")/.." && pwd)"
py="${ARC_VOICE_PYTHON:-$HOME/.local/share/arc/venv/bin/python}"
out="$(mktemp)"; err="$(mktemp)"
trap 'rm -f "$out" "$err"' EXIT

feed() {
    sleep 1.5
    echo '{"action":"start_listening"}'
    sleep "$secs"
    echo '{"action":"stop_listening"}'
    sleep 3
    echo '{"action":"shutdown"}'
}

feed | PYTHONPATH="$root/python" timeout $((secs + 30)) "$py" -m arc_voice >"$out" 2>"$err" || true

grep -v '"level"' "$out" | cut -c1-220 || true
levels=$(grep -c '"level"' "$out" || true)
peak=$(grep -o '"rms":[0-9.]*' "$out" | cut -d: -f2 | sort -g | tail -1 || true)
echo "levels: $levels  peak rms: ${peak:-none}"
echo "--- stderr"
cat "$err"
