#!/bin/bash
# Update efd-station on the CM5 Pi: pull, build, package, install.
# Run this ON the Pi.
#
# Usage:
#   ./scripts/update-pi.sh          # full update
#   ./scripts/update-pi.sh --quick  # build only, skip packaging

set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_DIR"

QUICK=false
if [[ "${1:-}" == "--quick" ]]; then
    QUICK=true
fi

echo "==> Pulling latest..."
git pull --ff-only

echo "==> Building server (release)..."
cargo build --release --package efd-server

if $QUICK; then
    echo "==> Quick mode: skipping package build"
else
    echo "==> Building .deb package..."
    if ! command -v cargo-deb &>/dev/null; then
        echo "    Installing cargo-deb..."
        cargo install cargo-deb
    fi
    cargo deb --package efd-server --no-build

    DEB=$(ls -t target/debian/efd-server_*.deb 2>/dev/null | head -1)
    if [[ -n "$DEB" ]]; then
        echo "==> Installing $DEB..."
        sudo dpkg -i "$DEB"
    else
        echo "!! No .deb found"
        exit 1
    fi
fi

echo "==> Stopping old server..."
sudo systemctl stop efd-server 2>/dev/null || true
# Kill any lingering instances (they hold USB/serial)
pkill -f 'efd-server' 2>/dev/null || true
sleep 1

echo "==> Starting efd-server service..."
sudo systemctl start efd-server 2>/dev/null || true

echo "==> Done."
echo "  Binary: target/release/efd-server"
echo "  Status: systemctl status efd-server"
