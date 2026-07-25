#!/usr/bin/env bash
# Stage 5.3: install the agentgrid control plane on a clean Linux host.
#
#   ./install-control-plane.sh [--listen 0.0.0.0:7800] [--data-dir /var/lib/agentgrid]
#
# Creates the unprivileged 'agentgrid' user, data + artifact directories, a
# systemd unit with hardened sandboxing (Stage 5.1), and starts the control
# plane. SQLite lives under DATA_DIR; the unit is the only long-running
# process. Requires systemd.
set -euo pipefail

LISTEN="${AGENTGRID_LISTEN:-0.0.0.0:7800}"
BIN_DIR="/usr/local/bin"
DATA_DIR="/var/lib/agentgrid"
DB_DIR="$DATA_DIR/data"
ARTIFACT_DIR="$DATA_DIR/artifacts"
BOOTSTRAP_USER="${AGENTGRID_BOOTSTRAP_USER:-admin}"
BOOTSTRAP_PASS="${AGENTGRID_BOOTSTRAP_PASSWORD:-changeme}"

usage() { echo "usage: $0 [--listen addr] [--data-dir dir] [--bin-dir dir]"; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --listen)    LISTEN="$2";    shift 2;;
    --data-dir)  DATA_DIR="$2";  shift 2
                 DB_DIR="$DATA_DIR/data"; ARTIFACT_DIR="$DATA_DIR/artifacts";;
    --bin-dir)   BIN_DIR="$2";   shift 2;;
    *) usage;;
  esac
done

command -v "$BIN_DIR/agentgrid-control-plane" >/dev/null 2>&1 || {
  echo "missing $BIN_DIR/agentgrid-control-plane (build + copy it first)"; exit 1;
}
command -v systemctl >/dev/null 2>&1 || { echo "systemd required"; exit 1; }

echo ">> creating user + directories"
if ! id agentgrid >/dev/null 2>&1; then useradd -r -m -d "$DATA_DIR" agentgrid; fi
mkdir -p "$DB_DIR" "$ARTIFACT_DIR"
chown -R agentgrid:agentgrid "$DATA_DIR"
chmod 700 "$DB_DIR"

echo ">> writing systemd unit"
cat > /etc/systemd/system/agentgrid-control-plane.service <<EOF
[Unit]
Description=agentgrid control plane
After=network-online.target
Wants=network-online.target

[Service]
User=agentgrid
Group=agentgrid
ExecStart=$BIN_DIR/agentgrid-control-plane
Restart=on-failure
RestartSec=5
Environment=AGENTGRID_LISTEN=$LISTEN
Environment=AGENTGRID_DATA_DIR=$DB_DIR
Environment=AGENTGRID_ARTIFACT_ROOT=$ARTIFACT_DIR
Environment=AGENTGRID_BOOTSTRAP_USER=$BOOTSTRAP_USER
Environment=AGENTGRID_BOOTSTRAP_PASSWORD=$BOOTSTRAP_PASS
# Hardening (Stage 5.1): no new privileges, read-only root except data dirs.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=$DATA_DIR
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

echo ">> enabling + starting"
systemctl daemon-reload
systemctl enable --now agentgrid-control-plane.service
echo ">> control plane listening on $LISTEN. journalctl -u agentgrid-control-plane -f"
echo ">> bootstrap user: $BOOTSTRAP_USER (change the password after first login)"
