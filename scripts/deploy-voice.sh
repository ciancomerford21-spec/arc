#!/usr/bin/env bash
# Re-deploy the voice sidecar from the working tree and restart the daemon.
#
# The service does NOT run the repo's python/arc_voice directly: it runs an
# installed copy at ~/.local/share/arc/python/arc_voice (see
# crates/arc-daemon/src/voice.rs, which points PYTHONPATH there). Editing the
# repo therefore does nothing until you run this.
#
# Use after changing anything under python/arc_voice/.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
data="${XDG_DATA_HOME:-$HOME/.local/share}/arc"

step() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

step "Syncing arc_voice -> $data/python/arc_voice"
rm -rf "$data/python/arc_voice"
mkdir -p "$data/python"
cp -r "$root/python/arc_voice" "$data/python/arc_voice"
find "$data/python" -name __pycache__ -type d -prune -exec rm -rf {} + 2>/dev/null || true

# The daemon is baked with a compile-time path, so a rebuild is needed if the
# package dir ever moves. Cheap no-op when unchanged.
if [[ $# -gt 0 && $1 == --rebuild ]]; then
    step "Rebuilding daemon + cli"
    cargo build --release --manifest-path "$root/Cargo.toml" -p arc-daemon -p arc-cli
    install -Dm755 "$root/target/release/arcd" "$HOME/.local/bin/arcd"
    install -Dm755 "$root/target/release/arc" "$HOME/.local/bin/arc"
fi

step "Restarting arcd"
systemctl --user restart arcd.service
sleep 3

if systemctl --user is-active --quiet arcd.service; then
    step "Done. Arc is running the current code."
    "$HOME/.local/bin/arc" status 2>/dev/null | grep -E "voice\.(mic|tts)" || true
else
    echo "arcd failed to start; check: journalctl --user -u arcd -n 30" >&2
    exit 1
fi
