#!/usr/bin/env bash
# Remove Arc's services, binaries and bar widget.
#   scripts/uninstall.sh          keep config, secrets, models and venv
#   scripts/uninstall.sh --purge  also delete ~/.config/arc and ~/.local/share/arc
set -euo pipefail

purge=0
[[ ${1:-} == --purge ]] && purge=1

cfg=${XDG_CONFIG_HOME:-$HOME/.config}
data=${XDG_DATA_HOME:-$HOME/.local/share}/arc

for u in arcd arc-llm; do
    systemctl --user disable --now "$u.service" 2>/dev/null || true
    rm -f "$cfg/systemd/user/$u.service"
done
systemctl --user daemon-reload

if command -v omarchy-bar >/dev/null && grep -q '"arc.status"' "$cfg/omarchy/shell.json" 2>/dev/null; then
    # Drop the widget entry from every bar section; the shell hot-reloads shell.json.
    python3 - "$cfg/omarchy/shell.json" <<'EOF'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
layout = d.get("bar", {}).get("layout", {})
for section, items in layout.items():
    if isinstance(items, list):
        layout[section] = [i for i in items if not (isinstance(i, dict) and i.get("id") == "arc.status")]
json.dump(d, open(p, "w"), indent=2)
EOF
fi
rm -rf "$cfg/omarchy/plugins/arc.status"

rm -f "$HOME/.local/bin/arc" "$HOME/.local/bin/arcd"
rm -rf "$data/python" "$data/bin/arc-llm"

if [[ $purge == 1 ]]; then
    rm -rf "$cfg/arc" "$data"
    echo "Arc removed, including config, secrets, models and venv."
else
    echo "Arc removed. Kept: $cfg/arc (config + secrets), $data (models, venv, llama.cpp)."
    echo "Run with --purge to delete those too."
fi
