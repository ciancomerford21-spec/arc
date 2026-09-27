#!/usr/bin/env bash
# Live check of arcd + the voice service on the real mic/speaker, using a
# private socket so it never collides with an installed daemon.
#   scripts/daemon-smoke.sh
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
arcd="$root/target/debug/arcd"
run="$(mktemp -d)"
cfg="$run/config.toml"
printf '[ai]\nprovider = "none"\n[voice]\nmode = "push_to_talk"\nspeak_text_replies = true\n' >"$cfg"
export ARC_SOCKET="$run/arc.sock" ARC_RUNTIME_DIR="$run"

"$arcd" --config "$cfg" 2>"$run/arcd.log" &
pid=$!
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null; rm -rf "$run"' EXIT

req() { printf '%s\n' "$1" | socat -t 15 - "UNIX-CONNECT:$ARC_SOCKET"; }

for _ in $(seq 60); do [[ -S $ARC_SOCKET ]] && break; sleep 0.25; done
# Wait for the voice service to report health.
for _ in $(seq 60); do
    req '{"id":1,"type":"status"}' | grep -q '"voice_mode":"push_to_talk"' && break
    sleep 0.5
done

echo "--- status (voice components)"
req '{"id":1,"type":"status"}' | python3 -c '
import json,sys
d=json.load(sys.stdin)["data"]
print("state:", d["state"], "| voice_mode:", d["voice_mode"], "| rss_kb:", d["rss_kb"])
for c in d["components"]: print("  %-12s %-12s %s" % (c["name"], c["status"], c["detail"][:60]))'
echo "--- ask (reply is spoken on the headset)"
req '{"id":2,"type":"ask","text":"battery status"}' | python3 -c '
import json,sys; d=json.load(sys.stdin)["data"]; print(d["route"], d["elapsed_ms"], "ms:", d["reply"][:140])'
sleep 4
echo "--- bar file"
cat "$run/bar.json"; echo
echo "--- arcd log"
cut -c1-160 "$run/arcd.log"
