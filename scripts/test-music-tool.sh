#!/usr/bin/env bash
# The playback tool is now a thin front door onto `arc music play`, so what is
# worth testing is what it says and when it fails.
#
# It used to resolve the search and start mpv itself, which needed stub yt-dlp
# and mpv on PATH plus a fake daemon to report to. All of that now happens
# inside the daemon, which is covered end to end against a stub player that
# speaks mpv's real IPC protocol (crates/arc-daemon/tests/daemon.rs). What is
# left here is the part that is still a shell script: the argument it requires,
# the exit code it returns, and the fact that a failure is said out loud rather
# than swallowed into a success.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$HERE/integrations/tools/play_youtube_music/run.sh"

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
  local what="$1" hay="$2" needle="$3"
  case "$hay" in
    *"$needle"*) pass=$((pass + 1)) ;;
    *)
      fail=$((fail + 1))
      echo "FAIL: $what" >&2
      echo "  output: $hay" >&2
      echo "  wanted to contain: $needle" >&2
      ;;
  esac
}
nonzero() {
  local what="$1" rc="$2"
  if [ "$rc" -ne 0 ]; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    echo "FAIL: $what (exited 0)" >&2
  fi
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
bin="$work/bin"
mkdir -p "$bin"

# A stub `arc` that records how it was called and answers with whatever the
# test told it to answer with.
cat >"$bin/arc" <<EOF
#!/usr/bin/env bash
if [ "\${1:-}" = "--json" ]; then shift; fi
echo "\$*" >> "$work/calls"
cat "$work/reply"
# The real CLI exits non-zero when the daemon answered with an error, and the
# script branches on that exit code rather than on the JSON.
grep -q '"error"' "$work/reply" && exit 1
exit 0
EOF
chmod +x "$bin/arc"
: >"$work/calls"

PLAYING='{"now":{"state":"playing","title":"Windowlicker","artist":"Aphex Twin","label":"Windowlicker — Aphex Twin","playing":true},"queue":[]}'
QUEUED='{"now":{"state":"playing","title":"Windowlicker","artist":"Aphex Twin","label":"Windowlicker — Aphex Twin","playing":true},"queue":[{"title":"Teardrop"},{"title":"Roygbiv"}]}'

run() {
  : >"$work/calls"
  ARC_COMMAND="$bin/arc" ARC_ARG_QUERY="${1-}" bash "$SCRIPT" >"$work/out" 2>&1
  RC=$?
  OUT="$(cat "$work/out")"
  CALLS="$(cat "$work/calls")"
}

# --- the happy path ---------------------------------------------------------
printf '%s' "$PLAYING" >"$work/reply"
run "aphex twin"
check "a played track exits 0" "$RC" "0"
contains "the real title is spoken, not the query" "$OUT" "Playing Windowlicker"
check "it asks the daemon to play the query verbatim" "$CALLS" "music play aphex twin"

# --- a queue behind the track is mentioned ----------------------------------
printf '%s' "$QUEUED" >"$work/reply"
run "aphex twin"
contains "a queued count is reported" "$OUT" "2 more queued"

# --- a refusal is a failure, and is said ------------------------------------
printf '%s' '{"error":{"message":"no playable result for \"kjsdfh\""}}' >"$work/reply"
run "kjsdfh"
nonzero "a refused search exits non-zero" "$RC"
contains "the daemon's reason survives to the user" "$OUT" "no playable result"

# The browser-fallback message is the daemon's now, not this script's. Losing
# it would mean silence after a request to play music, which reads as a bug.
printf '%s' '{"error":{"message":"no stream. Opened a YouTube Music search page instead"}}' >"$work/reply"
run "obscure thing"
nonzero "the fallback is still a non-zero exit" "$RC"
contains "the fallback is reported, not hidden" "$OUT" "search page"

# --- a daemon that answers with no track ------------------------------------
printf '%s' '{"now":{"state":"stopped","playing":false}}' >"$work/reply"
run "aphex twin"
nonzero "a reply with no track is a failure" "$RC"

# --- no query ---------------------------------------------------------------
printf '%s' "$PLAYING" >"$work/reply"
run ""
nonzero "no query exits non-zero" "$RC"
contains "no query says so" "$OUT" "no query"
check "no query does not reach the daemon" "$CALLS" ""

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]