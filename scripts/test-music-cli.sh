#!/usr/bin/env bash
# Drive the real `arc music` CLI against a real `arcd`, the way the Arc app and
# the playback script do. This is the link the shell test cannot cover:
# argument parsing, the socket, the daemon, and back out to something a person
# can read.
#
# The player and resolver are stubs that speak mpv's real IPC protocol, so no
# audio is played and no network is touched -- but everything between the
# command line and the player is the real code path.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARC="$ROOT/target/debug/arc"
ARCD="$ROOT/target/debug/arcd"
[ -x "$ARC" ] && [ -x "$ARCD" ] || { echo "build first: cargo build" >&2; exit 1; }

work="$(mktemp -d)"
mkdir -p "$work/config" "$work/run"
export ARC_SOCKET="$work/run/arc.sock"
export ARC_RUNTIME_DIR="$work/run"
export ARC_CONFIG_DIR="$work/nonexistent"
export ARC_MEMORY_PATH="$work/memory"
export ARC_AUTOMATIONS="$work/automations.toml"
export ARC_TOOLS_DIR="$work/tools"
export ARC_TOOL_CLASSES="$work/tool_classes.json"

# A player that speaks mpv's JSON IPC, and a resolver that prints one line of
# `--print` output per result. Same shape as the daemon's own end-to-end stub.
cat >"$work/player" <<'PY'
#!/usr/bin/env python3
import json, os, socket, sys
sock = sys.argv[sys.argv.index("--input-ipc-server") + 1]
if os.path.exists(sock):
    os.remove(sock)
srv = socket.socket(socket.AF_UNIX); srv.bind(sock); srv.listen(1)
conn, _ = srv.accept()
buf = b""; paused = False
while True:
    data = conn.recv(65536)
    if not data:
        break
    buf += data
    while b"\n" in buf:
        line, buf = buf.split(b"\n", 1)
        if not line.strip():
            continue
        req = json.loads(line); cmd = req.get("command", [])
        name = cmd[0] if cmd else ""
        if name == "get_property":
            data = {"playlist-pos": 0, "pause": paused, "time-pos": 7.0,
                    "duration": 233.0, "idle-active": False}.get(cmd[1])
        elif name == "set_property" and cmd[1] == "pause":
            paused = bool(cmd[2]); data = None
        else:
            data = None
        conn.sendall((json.dumps({"data": data, "request_id": req.get("request_id"),
                                 "error": "success"}) + "\n").encode())
PY
chmod +x "$work/player"

cat >"$work/resolver" <<'PY'
#!/usr/bin/env python3
import sys
args = sys.argv[1:]
query, n = "", 1
for a in args:
    if a.startswith("ytsearch"):
        query = a.split(":", 1)[1]
        n = int(a[len("ytsearch"):].split(":")[0] or 1)
for i in range(n):
    print("%s %d|||Boards of Canada|||https://example.invalid/stream%d" % (query, i, i))
PY
chmod +x "$work/resolver"

printf '[ai]\nprovider = "none"\n[music]\nenabled = true\nbrowser_fallback = false\nplayer = "%s"\nresolver = "%s"\n' \
  "$work/player" "$work/resolver" >"$work/config/config.toml"

"$ARCD" --config "$work/config/config.toml" --no-voice --no-bar >"$work/log" 2>&1 &
arcd_pid=$!
trap 'kill "$arcd_pid" 2>/dev/null; rm -rf "$work"' EXIT

for _ in $(seq 1 100); do
  [ -S "$ARC_SOCKET" ] && break
  sleep 0.1
done
[ -S "$ARC_SOCKET" ] || { echo "arcd never created its socket" >&2; cat "$work/log" >&2; exit 1; }

pass=0
fail=0
check() {
  local what="$1" got="$2" want="$3"
  if [ "$got" = "$want" ]; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    echo "FAIL: $what" >&2
    echo "  got:  $got" >&2
    echo "  want: $want" >&2
  fi
}
contains() {
  case "$2" in
    *"$3"*) pass=$((pass + 1)) ;;
    *) fail=$((fail + 1)); echo "FAIL: $1" >&2; echo "  output: $2" >&2 ;;
  esac
}
nonzero() {
  if [ "$2" -ne 0 ]; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    echo "FAIL: $1 exited 0" >&2
  fi
}
field() { python3 -c "import json,sys;d=json.load(sys.stdin);print($1)"; }

# --- reads answer with nothing playing --------------------------------------
check "nothing playing at first" "$("$ARC" music)" "nothing playing"
check "and the JSON says stopped" \
  "$("$ARC" --json music show | field 'd["now"]["state"]')" "stopped"

# --- play -------------------------------------------------------------------
"$ARC" music play "hall of fame" >/dev/null
check "the real title is shown, not the query" "$("$ARC" music)" "hall of fame 0 — Boards of Canada"
check "the artist survives quoting and dashes" \
  "$("$ARC" --json music show | field 'd["now"]["artist"]')" "Boards of Canada"
check "the stream url never reaches a client" \
  "$("$ARC" --json music show | grep -c 'example.invalid/stream')" "0"
# The playhead is polled by the daemon's supervisor once a second, so the
# duration is not there the instant `play` returns. Poll for it rather than
# sleeping a fixed time, which would either be flaky or needlessly slow.
dur=""
for _ in $(seq 1 40); do
  dur="$("$ARC" --json music position | field 'int(d["duration"])')"
  [ "$dur" != "0" ] && break
  sleep 0.25
done
check "the playhead is reported separately, once the supervisor has ticked" "$dur" "233"

# --- queue ------------------------------------------------------------------
"$ARC" music enqueue "teardrop" >/dev/null
check "the queue is listed" "$("$ARC" music | grep -c 'queued: teardrop 0 — Boards of Canada')" "1"
check "the playing track did not change" "$("$ARC" music | head -1)" "hall of fame 0 — Boards of Canada"
"$ARC" music remove 0 >/dev/null
check "removing the queued track empties the queue" \
  "$("$ARC" --json music show | field 'len(d["queue"])')" "0"
check "and left the playing track alone" "$("$ARC" music | head -1)" "hall of fame 0 — Boards of Canada"

# --- transport --------------------------------------------------------------
"$ARC" music pause >/dev/null
check "pausing is reported as paused" \
  "$("$ARC" --json music show | field 'd["now"]["state"]')" "paused"
check "a paused track still shows" \
  "$("$ARC" --json music show | field 'd["now"]["playing"]')" "True"
check "the plain output says it is paused" \
  "$("$ARC" music | head -1)" "hall of fame 0 — Boards of Canada  (paused)"
"$ARC" music resume >/dev/null
check "resuming" "$("$ARC" --json music show | field 'd["now"]["state"]')" "playing"
"$ARC" music toggle >/dev/null
check "toggling pauses" "$("$ARC" --json music show | field 'd["now"]["state"]')" "paused"
"$ARC" music toggle >/dev/null
check "toggling again resumes" "$("$ARC" --json music show | field 'd["now"]["state"]')" "playing"

# --- stop -------------------------------------------------------------------
"$ARC" music stop >/dev/null
check "stopping clears it" "$("$ARC" music)" "nothing playing"

# --- refusals ---------------------------------------------------------------
"$ARC" music play "x" >/dev/null
"$ARC" music next >/dev/null 2>&1
nonzero "skipping with nothing queued fails" $?
"$ARC" music remove 9 >/dev/null 2>&1
nonzero "removing a missing entry fails" $?
contains "play with no query says what is missing" "$("$ARC" music play 2>&1)" "needs something to search for"
"$ARC" music play >/dev/null 2>&1
nonzero "play with no query fails" $?
"$ARC" music frobnicate >/dev/null 2>&1
nonzero "an unknown action fails" $?
"$ARC" music stop >/dev/null

# --- no daemon --------------------------------------------------------------
# Stopping is local state, so a playback script that finds no socket must not
# fail because of it.
ARC_SOCKET="$work/run/missing.sock" "$ARC" --json music stop >/dev/null 2>&1
check "stop works with no daemon" "$?" "0"
# A read with no daemon is an error rather than a lie about what is playing.
ARC_SOCKET="$work/run/missing.sock" "$ARC" --json music show >/dev/null 2>&1
nonzero "show with no daemon fails loudly" $?

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]