#!/usr/bin/env bash
# Fast incremental deploy to CM5 — skips makepkg, reuses cargo cache.
# Usage: ./dist/deploy-server.sh [user@host]
set -euo pipefail

HOST="${1:-efd@efd-pi}"
BINARY="efd-server"

echo "==> Deploying to $HOST"
ssh "$HOST" bash -s <<'REMOTE'
set -euo pipefail
cd ~/efd-station || { echo "Clone repo first: git clone ... ~/efd-station"; exit 1; }
git pull --ff-only
echo "==> Building (incremental)..."
cargo build --release --package efd-server 2>&1 | tail -5
echo "==> Installing..."
sudo install -m755 target/release/efd-server /usr/bin/efd-server
echo "==> Restarting..."
sudo systemctl restart efd-server
sleep 1
systemctl is-active --quiet efd-server && echo "==> OK: efd-server running" || echo "==> FAIL: check journalctl -u efd-server"
REMOTE
