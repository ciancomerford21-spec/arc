#!/usr/bin/env bash
# Install Arc for the current user (no root needed).
#
#   scripts/install.sh            build (release), install, enable + start services, add the bar widget
#   scripts/install.sh --no-bar   skip the Omarchy bar widget
#   scripts/install.sh --debug    install debug builds (faster to build)
#
# Layout:
#   ~/.local/bin/{arc,arcd}                     binaries
#   ~/.local/share/arc/python/arc_voice         voice sidecar (venv + models already in ~/.local/share/arc)
#   ~/.local/share/arc/bin/arc-llm              llama-server launcher
#   ~/.config/systemd/user/{arcd,arc-llm,arc-ui}.service
#   ~/.config/omarchy/plugins/arc.status        bar widget
#   ~/.config/arc/config.toml                   only created if missing
set -euo pipefailq

root="$(cd "$(dirname "$0")/.." && pwd)"
profile=release bar=1
for a in "$@"; do
    case $a in
        --no-bar) bar=0 ;;
        --debug) profile=debug ;;
        -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
        *) echo "unknown option: $a" >&2; exit 2 ;;
    esac
done

bin=$HOME/.local/bin
data=${XDG_DATA_HOME:-$HOME/.local/share}/arc
conf=${XDG_CONFIG_HOME:-$HOME/.config}/arc
units=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
plugins=${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins

step() { printf '\033[1m==> %s\033[0m\n' "$*"; }

step "Checking prerequisites"
for c in cargo pw-cat; do command -v "$c" >/dev/null || { echo "missing: $c" >&2; exit 1; }; done
[[ -x $data/venv/bin/python ]] || { echo "missing voice venv: $data/venv (see README: voice setup)" >&2; exit 1; }
"$root/scripts/fetch-models.sh" --verify >/dev/null || { echo "voice models missing: run scripts/fetch-models.sh" >&2; exit 1; }

step "Building ($profile)"
# The GTK panel is optional: it needs gtk4 and gtk4-layer-shell, which are a
# much heavier dependency than the daemon, so a missing system GTK must not
# fail the whole install.
build_ui=1
if ! pkg-config --exists gtk4 2>/dev/null; then
    echo "  gtk4 not found; skipping the arc-ui panel" >&2
    build_ui=0
fi
pkgs="-p arc-daemon -p arc-cli"
[[ $build_ui == 1 ]] && pkgs="$pkgs -p arc-ui"
if [[ $profile == release ]]; then
    # shellcheck disable=SC2086
    cargo build --release --manifest-path "$root/Cargo.toml" $pkgs
else
    # shellcheck disable=SC2086
    cargo build --manifest-path "$root/Cargo.toml" $pkgs
fi

step "Installing files"
install -Dm755 "$root/target/$profile/arcd" "$bin/arcd"
install -Dm755 "$root/target/$profile/arc" "$bin/arc"
if [[ $build_ui == 1 ]]; then
    install -Dm755 "$root/target/$profile/arc-ui" "$bin/arc-ui"
fi
install -Dm755 "$root/scripts/arc-llm.sh" "$data/bin/arc-llm"
rm -rf "$data/python/arc_voice"
mkdir -p "$data/python"
cp -r "$root/python/arc_voice" "$data/python/arc_voice"
find "$data/python" -name __pycache__ -prune -exec rm -rf {} +
install -Dm644 "$root/README.md" "$data/README.md" 2>/dev/null || true

mkdir -p "$conf"; chmod 700 "$conf"
if [[ ! -f $conf/config.toml ]]; then
    install -m644 "$root/config/config.toml" "$conf/config.toml"
    echo "   created $conf/config.toml"
else
    echo "   kept existing $conf/config.toml"
fi
[[ -f $conf/secrets.env ]] && chmod 600 "$conf/secrets.env"

install -Dm644 "$root/systemd/arcd.service" "$units/arcd.service"
install -Dm644 "$root/systemd/arc-llm.service" "$units/arc-llm.service"
if [[ $build_ui == 1 ]]; then
    install -Dm644 "$root/systemd/arc-ui.service" "$units/arc-ui.service"
fi

step "Starting services"
systemctl --user daemon-reload
# Only run the local model server if the config actually uses it.
if grep -qE '^(provider|fallback)[[:space:]]*=[[:space:]]*"local"' "$conf/config.toml"; then
    systemctl --user enable --now arc-llm.service
else
    echo "   local model not used by config; arc-llm.service left disabled"
fi
systemctl --user enable arcd.service
systemctl --user restart arcd.service
if [[ $build_ui == 1 ]]; then
    systemctl --user enable --now arc-ui.service
else
    echo "   panel not built; arc-ui.service not installed"
fi

for _ in $(seq 40); do "$bin/arc" ping >/dev/null 2>&1 && break; sleep 0.25; done
"$bin/arc" ping || { echo "arcd did not come up; see: journalctl --user -u arcd -e" >&2; exit 1; }

if [[ $bar == 1 ]]; then
    if command -v omarchy-bar >/dev/null && [[ -d $(dirname "$plugins") ]]; then
        step "Adding the Omarchy bar widget"
        rm -rf "$plugins/arc.status"
        mkdir -p "$plugins"
        cp -r "$root/integrations/omarchy-bar/arc.status" "$plugins/arc.status"
        omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true
        if ! grep -q '"arc.status"' "${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/shell.json" 2>/dev/null; then
            omarchy bar put arc.status --after omarchy.tray || omarchy bar put arc.status --section right
        fi
        omarchy bar set arc.status arcCommand "$bin/arc"
    else
        echo "   Omarchy shell not found; skipping bar widget"
    fi
fi

step "Done"
"$bin/arc" status | head -3
cat <<EOF

Arc is running (systemctl --user status arcd).
  Talk:      say "Hey Arc, ..." or click the bar icon
  Logs:      journalctl --user -u arcd -f
  Uninstall: $root/scripts/uninstall.sh
EOF
