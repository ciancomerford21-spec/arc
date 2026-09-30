#!/usr/bin/env bash
# Everything: the Rust workspace, the shell tool's reporting, the CLI against a
# real daemon, and the QML overlay loading in the real Quickshell engine.
#
# The four kinds are separate because they fail differently. `cargo test` does
# not run the playback script, the script test does not start a daemon, and
# neither one notices if the overlay stopped parsing -- which is a real failure
# mode for a Quickshell UI, since a syntax error there is a blank window and
# nothing in the logs.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

steps=(
  "cargo test --workspace|cargo test --workspace"
  "scripts/test-music-tool.sh|bash scripts/test-music-tool.sh"
  "scripts/test-now-playing-cli.sh|bash scripts/test-now-playing-cli.sh"
  "scripts/check-qml.sh|bash scripts/check-qml.sh"
)

fail=0
for step in "${steps[@]}"; do
  label="${step%%|*}"
  command="${step#*|}"
  echo "=== $label"
  if bash -c "$command"; then
    echo "--- PASS: $label"
  else
    echo "--- FAIL: $label"
    fail=$((fail + 1))
  fi
  echo
done

if [ "$fail" -eq 0 ]; then
  echo "all ${#steps[@]} checks passed"
else
  echo "$fail of ${#steps[@]} checks FAILED"
fi
exit "$fail"