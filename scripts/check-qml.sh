#!/usr/bin/env bash
# Load every QML config through the real Quickshell engine and fail on any
# error it reports.
#
# A structural brace check was tried first and threw false positives: QML
# embeds JavaScript, whose regex literals and strings contain braces and quotes
# that a hand-rolled lexer cannot tell from code. The engine parses QML
# properly and says what is wrong, so use the engine. Quickshell only builds
# the object tree on load, which is exactly the failure mode being caught here:
# an unbalanced brace in a 1000-line overlay otherwise shows up as a blank
# window with nothing in the logs.
#
# `timeout` is not optional: ShellRoot keeps running after a successful load,
# so without it this hangs forever on a file that loaded fine.
#
# Run: scripts/check-qml.sh [config-dir ...]
set -uo pipefail

command -v qs >/dev/null 2>&1 || { echo "qs not found; cannot load QML" >&2; exit 1; }

# Only standalone configs are loadable here. The Omarchy bar widget
# (integrations/omarchy-bar/arc.status/Panel.qml) is not: it imports `qs.Commons`
# and `qs.Ui`, which only the Omarchy shell provides, so `qs -c` cannot parse it
# outside that shell. It is not checked here rather than being reported as a
# failure it is not.
dirs=("$@")
[ ${#dirs[@]} -gt 0 ] || dirs=("$(dirname "$0")/../integrations/arc-app")

pass=0
fail=0
for dir in "${dirs[@]}"; do
  [ -d "$dir" ] || { echo "no such config dir: $dir" >&2; fail=$((fail + 1)); continue; }
  log="$(mktemp)"
  # offscreen so this runs without a compositor; the engine still parses, which
  # is what is being checked.
  QT_QPA_PLATFORM=offscreen timeout 15 qs -c "$(realpath "$dir")" >"$log" 2>&1
  # Quickshell was killed by the timeout, which is expected -- a loaded config
  # does not exit on its own. What matters is what it printed.
  if grep -qiE "^(ERROR|FATAL)|error:|is not a|Expected token|Unexpected token|Cannot assign|Unable to assign" "$log"; then
    fail=$((fail + 1))
    echo "FAIL: $dir" >&2
    grep -iE "^(ERROR|FATAL)|error:|is not a|Expected token|Unexpected token|Cannot assign|Unable to assign" "$log" >&2
  elif grep -q "Configuration Loaded" "$log"; then
    pass=$((pass + 1))
    echo "ok:   $dir"
  else
    fail=$((fail + 1))
    echo "FAIL: $dir never reported Configuration Loaded" >&2
    cat "$log" >&2
  fi
  rm -f "$log"
done

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]