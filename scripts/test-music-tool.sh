#!/usr/bin/env bash
# The playback script must report to the daemon only once something is really
# playing, and must never leave a title up for a track that never started.
#
# The script is the only thing that knows what actually happened -- whether
# yt-dlp found a stream, whether mpv launched -- so this runs the real script
# with stub yt-dlp/mpv/arc on PATH and checks what it told the stub daemon.
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

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
bin="$work/bin"
mkdir -p "$bin"

# A stub `arc` that records every now-playing call as one line.
cat >"$bin/arc" <<EOF
#!/usr/bin/env bash
# \$*: the flags arc was called with, minus the leading --json
if [ "\${1:-}" = "--json" ]; then shift; fi
echo "\$*" >> "$work/calls"
echo '{}'
EOF
chmod +x "$bin/arc"

# A stub yt-dlp: -g prints a stream URL, --print prints one field per call.
cat >"$bin/yt-dlp" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  *-g*) echo "https://example.invalid/stream.m4a" ;;
  *"%(title)s"*) echo "Hall of Fame" ;;
  *"%(uploader)s"*) echo "Boards of Canada" ;;
esac
EOF
chmod +x "$bin/yt-dlp"

# A stub mpv that records its pid and stays alive until told otherwise.
cat >"$bin/mpv" <<EOF
#!/usr/bin/env bash
echo "\$\$" > "$work/mpv_pid"
sleep 30
EOF
chmod +x "$bin/mpv"

# A stub xdg-open, present but never wanted in the success case.
cat >"$bin/xdg-open" <<EOF
#!/usr/bin/env bash
echo "\$*" > "$work/xdg_called"
EOF
chmod +x "$bin/xdg-open"

export PATH="$bin:$PATH"
export ARC_ARG_QUERY="Boards of Canada Hall of Fame"

# ---------------------------------------------------------------- success
out="$(bash "$SCRIPT")"
calls="$(cat "$work/calls")"
check "plays with mpv" "$(printf '%s' "$out" | grep -c 'Playing locally with mpv')" "1"
check "uses the real title, not the query" \
  "$(printf '%s' "$out" | grep -c 'Hall of Fame — Boards of Canada')" "1"
check "reports one set" "$(printf '%s\n' "$calls" | grep -c 'now-playing set')" "1"
check "reports the title" \
  "$(printf '%s\n' "$calls" | grep -c -- '--title Hall of Fame')" "1"
check "reports the artist" \
  "$(printf '%s\n' "$calls" | grep -c -- '--artist Boards of Canada')" "1"
check "reports the source" \
  "$(printf '%s\n' "$calls" | grep -c -- '--source youtube music')" "1"
# The pid must be mpv's own, or the daemon cannot tell when the track ends.
mpv_pid="$(cat "$work/mpv_pid" 2>/dev/null)"
check "reports mpv's pid" \
  "$(printf '%s\n' "$calls" | grep -c -- "--pid $mpv_pid")" "1"
check "the browser fallback was not used" \
  "$([ -f "$work/xdg_called" ] && echo yes || echo no)" "no"
# And nothing must kill it during the test; the stub mpv sleeps 30s.
check "mpv is still running" "$(kill -0 "$mpv_pid" 2>/dev/null && echo yes || echo no)" "yes"
kill "$mpv_pid" 2>/dev/null
wait 2>/dev/null

# A stale track must be cleared even when the request itself was unusable --
# "play " with no query is exactly when the overlay would otherwise keep showing
# the last thing that played.
: >"$work/calls"
out="$(ARC_ARG_QUERY="" bash "$SCRIPT")"
check "an empty query fails" "$(printf '%s' "$out" | grep -c 'ERROR: no query')" "1"
check "an empty query clears the row" \
  "$(printf '%s\n' "$(cat "$work/calls")" | grep -c 'now-playing stop')" "1"
check "an empty query never reports a track" \
  "$(printf '%s\n' "$(cat "$work/calls")" | grep -c 'now-playing set')" "0"

# ---------------------------------------- yt-dlp finds nothing: clear, don't set
: >"$work/calls"
cat >"$bin/yt-dlp" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  *"%(title)s"*) echo "Hall of Fame" ;;
  *"%(uploader)s"*) echo "Boards of Canada" ;;
  *-g*) : ;;
esac
EOF
chmod +x "$bin/yt-dlp"
out="$(bash "$SCRIPT")"
check "no stream means no mpv" "$(printf '%s' "$out" | grep -c 'found no stream')" "1"
check "a failed start reports stop" \
  "$(printf '%s\n' "$(cat "$work/calls")" | grep -c 'now-playing stop')" "1"
check "a failed start never reports a track" \
  "$(printf '%s\n' "$(cat "$work/calls")" | grep -c 'now-playing set')" "0"

# ---------------------------------------- browser fallback: clear, never claim
: >"$work/calls"
cat >"$bin/xdg-open" <<EOF
#!/usr/bin/env bash
exit 0
EOF
chmod +x "$bin/xdg-open"
# A PATH with no mpv and no yt-dlp at all. /usr/bin cannot be on it: on this
# machine the real mpv and yt-dlp live there, and the test would silently
# exercise the success path instead of the fallback it is meant to check. The
# coreutils the script needs are symlinked in individually.
bare="$work/bare"
mkdir -p "$bare"
for tool in bash python3 head cat tr sed grep; do
  real="$(command -v "$tool" 2>/dev/null)" || continue
  ln -sf "$real" "$bare/$tool"
done
ln -sf "$bin/arc" "$bare/arc"
ln -sf "$bin/xdg-open" "$bare/xdg-open"
check "the stub PATH really has no mpv" \
  "$(PATH="$bare" command -v mpv >/dev/null 2>&1 && echo yes || echo no)" "no"
check "the stub PATH really has no yt-dlp" \
  "$(PATH="$bare" command -v yt-dlp >/dev/null 2>&1 && echo yes || echo no)" "no"
out="$(PATH="$bare" ARC_COMMAND="$bare/arc" bash "$SCRIPT")"
check "the browser is used when there is no player" \
  "$(printf '%s' "$out" | grep -c 'Opened YouTube Music search')" "1"
# Nothing Arc started is playing, so claiming a track would be a lie.
check "the browser fallback reports stop" \
  "$(printf '%s\n' "$(cat "$work/calls")" | grep -c 'now-playing stop')" "1"
check "the browser fallback never reports a track" \
  "$(printf '%s\n' "$(cat "$work/calls")" | grep -c 'now-playing set')" "0"

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]