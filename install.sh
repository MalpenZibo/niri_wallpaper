#!/usr/bin/env bash
# Build and install niri-wallpaper.
# Usage: ./install.sh [--restart]
#   --restart   also kill the running daemon and respawn it detached
# Override the install prefix with PREFIX=/usr/local sudo -E ./install.sh.

set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"

DEST="${PREFIX:-$HOME/.local}/bin/niri-wallpaper"

cargo build --release
install -Dm755 target/release/niri-wallpaper "$DEST"
echo "installed: $DEST"

if [[ "${1-}" == "--restart" ]]; then
    pkill -x niri-wallpaper 2>/dev/null && sleep 0.3 || true
    setsid -f "$DEST" >/dev/null 2>&1
    echo "daemon restarted"
elif pgrep -x niri-wallpaper >/dev/null; then
    echo "daemon is running — restart with: ./install.sh --restart"
fi
