#!/bin/bash
# One-shot migration: move efd-server.service from running as the `efd`
# system user to running as `mikel`, so the DRM bridge can reach the
# per-user PipeWire socket at /run/user/1000/pulse/native.
#
# Run on the Pi, from the repo root:
#     sudo bash scripts/migrate-service-to-mikel.sh
#
# Idempotent: safe to re-run. Each step checks for the work already being
# done and skips if so.

set -euo pipefail

TARGET_USER=mikel
TARGET_UID=1000
OLD_USER=efd

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
UNIT_SRC="$REPO_DIR/dist/systemd/efd-server.service"
UNIT_DST=/usr/lib/systemd/system/efd-server.service
OLD_CONFIG_DIR=/home/$OLD_USER/.config/efd-backend
NEW_CONFIG_DIR=/home/$TARGET_USER/.config/efd-backend

if [[ $EUID -ne 0 ]]; then
    echo "!! Must run as root: sudo bash $0" >&2
    exit 1
fi

if ! id "$TARGET_USER" >/dev/null 2>&1; then
    echo "!! Target user $TARGET_USER does not exist" >&2
    exit 1
fi

if [[ ! -f "$UNIT_SRC" ]]; then
    echo "!! Updated unit file not found at $UNIT_SRC" >&2
    echo "   Run from the repo root (or git pull first)." >&2
    exit 1
fi

echo "==> 1. Enable linger for $TARGET_USER (so /run/user/$TARGET_UID persists)"
if loginctl show-user "$TARGET_USER" 2>/dev/null | grep -q '^Linger=yes$'; then
    echo "    already enabled"
else
    loginctl enable-linger "$TARGET_USER"
    echo "    enabled"
fi

echo "==> 2. Add $TARGET_USER to dialout/audio/plugdev groups"
for g in dialout audio plugdev; do
    if id -nG "$TARGET_USER" | tr ' ' '\n' | grep -qx "$g"; then
        echo "    $g: already a member"
    else
        usermod -aG "$g" "$TARGET_USER"
        echo "    $g: added"
    fi
done

echo "==> 3. Copy backend config from $OLD_CONFIG_DIR to $NEW_CONFIG_DIR"
if [[ -f "$NEW_CONFIG_DIR/config.toml" ]]; then
    echo "    $NEW_CONFIG_DIR/config.toml already present, leaving as is"
elif [[ -f "$OLD_CONFIG_DIR/config.toml" ]]; then
    install -d -o "$TARGET_USER" -g "$TARGET_USER" -m 0755 "$NEW_CONFIG_DIR"
    install -o "$TARGET_USER" -g "$TARGET_USER" -m 0644 \
        "$OLD_CONFIG_DIR/config.toml" "$NEW_CONFIG_DIR/config.toml"
    echo "    copied"
else
    echo "!! No existing config at $OLD_CONFIG_DIR/config.toml to migrate." >&2
    echo "   Create $NEW_CONFIG_DIR/config.toml by hand, then re-run." >&2
    exit 1
fi

echo "==> 4. Stop efd-server before swapping the unit"
systemctl stop efd-server.service 2>/dev/null || true

echo "==> 5. Install updated unit file"
install -m 0644 "$UNIT_SRC" "$UNIT_DST"
systemctl daemon-reload

echo "==> 6. Verify PipeWire is reachable for $TARGET_USER"
if runuser -u "$TARGET_USER" -- \
        env XDG_RUNTIME_DIR="/run/user/$TARGET_UID" pactl info >/dev/null 2>&1; then
    echo "    pactl info OK"
else
    echo "!! pactl info failed for $TARGET_USER — PipeWire session not reachable." >&2
    echo "   Check: loginctl user-status $TARGET_USER" >&2
    echo "   Check: systemctl --user --machine=$TARGET_USER@ status pipewire pipewire-pulse" >&2
    exit 1
fi

echo "==> 7. Start efd-server"
systemctl start efd-server.service
sleep 1
systemctl --no-pager --lines=0 status efd-server.service || true

echo
echo "Done. Tail the log to see the DRM bridge start:"
echo "    journalctl -u efd-server -f"
