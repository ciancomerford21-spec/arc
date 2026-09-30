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
#   ~/.config/systemd/user/{arcd,hermes-proxy}.service
#   ~/.config/omarchy/plugins/arc.status        bar widget
#   ~/.config/quickshell/arc                    Arc app (launch: qs -c arc)
#   ~/.local/share/applications/arc.desktop     app launcher entry
#   ~/.config/arc/config.toml                   only created if missing
set -euo pipefail

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
if [[ $profile == release ]]; then
    cargo build --release --manifest-path "$root/Cargo.toml" -p arc-daemon -p arc-cli
else
    cargo build --manifest-path "$root/Cargo.toml" -p arc-daemon -p arc-cli
fi

step "Installing files"
install -Dm755 "$root/target/$profile/arcd" "$bin/arcd"
install -Dm755 "$root/target/$profile/arc" "$bin/arc"
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
install -Dm644 "$root/systemd/hermes-proxy.service" "$units/hermes-proxy.service"

step "Starting services"
systemctl --user daemon-reload
# The proxy is Arc's only reasoning provider: without it Arc cannot answer.
systemctl --user enable --now hermes-proxy.service
systemctl --user enable arcd.service
systemctl --user restart arcd.service

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

if command -v qs >/dev/null; then
    step "Installing the Arc app"
    qsdir="${XDG_CONFIG_HOME:-$HOME/.config}/quickshell/arc"
    rm -rf "$qsdir"
    mkdir -p "$qsdir"
    cp "$root/integrations/arc-app/shell.qml" "$qsdir/shell.qml"
    # The app runs `arc` by name; point it at the installed binary.
    sed -i "s|readonly property string arcCmd: \"arc\"|readonly property string arcCmd: \"$bin/arc\"|" "$qsdir/shell.qml"
    apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
    mkdir -p "$apps"
    cat >"$apps/arc.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Arc
Comment=Arc assistant: tools, chat and thought process
Exec=qs -c arc
Icon=utilities-terminal
Terminal=false
Categories=Utility;
EOF
else
    echo "   quickshell not found; skipping the Arc app"
fi

step "Done"
"$bin/arc" status | head -3
cat <<EOF

Arc is running (systemctl --user status arcd).
  Talk:      say "Hey Arc, ..." or click the bar icon
  Logs:      journalctl --user -u arcd -f
  Uninstall: $root/scripts/uninstall.sh
EOF
