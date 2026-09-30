#!/usr/bin/env bash
# Install the self-made tools in integrations/tools/ into the live tool
# directory, where the daemon loads them from at start.
#
# The repo copy is the source of truth: a tool lives in ~/.local/share/arc/
# tools/ so the daemon can run it, but it also lives here so it is reviewable
# and survives a wiped data directory. This script syncs repo -> live.
#
# The fingerprint in tool.json is recomputed rather than carried, because the
# daemon compares it against the script text to decide whether a script was
# edited after the user reviewed it. A stale fingerprint would either block a
# legitimate new classification or, worse, keep an old one on text the user
# never read.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${ARC_TOOLS_DIR:-$HOME/.local/share/arc/tools}"

[ -d "$HERE/integrations/tools" ] || { echo "no integrations/tools in $HERE" >&2; exit 1; }
mkdir -p "$DEST"

fingerprint() {
  # FNV-1a 64-bit, matching `fingerprint()` in crates/arc-tools/src/custom.rs.
  python3 - "$1" <<'PY'
import sys
h = 0xcbf29ce484222325
for b in open(sys.argv[1], 'rb').read():
    h ^= b
    h = (h * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
print(f"{h:016x}")
PY
}

installed=0
for src in "$HERE"/integrations/tools/*/; do
  name="$(basename "$src")"
  [ -f "$src/tool.json" ] || continue
  dir="$DEST/$name"
  mkdir -p "$dir"
  # run.sh is the reviewed text; keep the fingerprint in step with it.
  python3 - "$src/tool.json" "$dir/tool.json" "$(fingerprint "$src/run.sh")" <<'PY'
import json, sys
src, dst, fp = sys.argv[1:4]
d = json.load(open(src))
d["fingerprint"] = fp
json.dump(d, open(dst, "w"), indent=2)
open(dst, "a").write("\n")
PY
  cp "$src/run.sh" "$dir/run.sh"
  chmod +x "$dir/run.sh"
  echo "installed $name -> $dir"
  installed=$((installed + 1))
done

echo "$installed tool(s) synced. Restart arcd to pick up changes."