#!/bin/bash
# One-command deploy: push from workstation, update Pi remotely, run client.
# Run this on your Manjaro workstation.
#
# Usage:
#   ./scripts/deploy.sh                    # push + update Pi + run client
#   ./scripts/deploy.sh --no-client        # push + update Pi only
#   ./scripts/deploy.sh --pi=user@host     # custom Pi SSH target
#
# Requires: ssh key auth to the Pi

set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_DIR"

PI_HOST="${EFD_PI_HOST:-mikel@fdmduopi.local}"
PI_REPO="${EFD_PI_REPO:-~/GitSpace/efd-station}"
WS_URL="${EFD_WS_URL:-ws://fdmduopi.local:8080/ws}"
RUN_CLIENT=true

for arg in "$@"; do
    case "$arg" in
        --no-client)  RUN_CLIENT=false ;;
        --pi=*)       PI_HOST="${arg#--pi=}" ;;
        --url=*)      WS_URL="${arg#--url=}" ;;
    esac
done

# Step 1: Build client locally
echo "==> Building client locally..."
cargo build --release --package efd-client

# Step 2: Update Pi remotely
echo "==> Updating Pi ($PI_HOST)..."
ssh "$PI_HOST" "cd $PI_REPO && git pull --ff-only && cargo build --release --package efd-server"

# Step 3: Restart server on Pi
echo "==> Restarting efd-server on Pi..."
ssh "$PI_HOST" "sudo systemctl restart efd-server 2>/dev/null || (cd $PI_REPO && target/release/efd-server &)"

# Give server a moment to start
sleep 2

# Step 4: Run client
if $RUN_CLIENT; then
    echo "==> Running client → $WS_URL"
    exec target/release/efd-client "$WS_URL"
else
    echo "==> Done. Run client manually:"
    echo "    target/release/efd-client $WS_URL"
fi
