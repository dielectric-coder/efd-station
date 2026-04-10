#!/bin/bash
# Update efd-station on Manjaro: pull, build client + server.
# Run this on your Manjaro workstation.
#
# Usage:
#   ./scripts/update-manjaro.sh            # build client only
#   ./scripts/update-manjaro.sh --package  # also build Arch package
#   ./scripts/update-manjaro.sh --run      # build and run client

set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_DIR"

PACKAGE=false
RUN=false
WS_URL="${EFD_WS_URL:-ws://fdmduopi.local:8080/ws}"

for arg in "$@"; do
    case "$arg" in
        --package) PACKAGE=true ;;
        --run)     RUN=true ;;
        --url=*)   WS_URL="${arg#--url=}" ;;
    esac
done

echo "==> Pulling latest..."
git pull --ff-only

echo "==> Building workspace (release)..."
cargo build --release --workspace

if $PACKAGE; then
    echo "==> Building Arch package..."
    cd dist/arch
    rm -rf src pkg efd-station
    makepkg -sf
    PKG=$(ls -t efd-server-*.pkg.tar.zst 2>/dev/null | head -1)
    if [[ -n "$PKG" ]]; then
        echo "==> Installing $PKG..."
        sudo pacman -U --noconfirm "$PKG"
    fi
    cd "$REPO_DIR"
fi

echo "==> Done."
echo "  Server: target/release/efd-server"
echo "  Client: target/release/efd-client"

if $RUN; then
    echo "==> Running client → $WS_URL"
    exec target/release/efd-client "$WS_URL"
fi
