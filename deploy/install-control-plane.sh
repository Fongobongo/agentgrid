#!/usr/bin/env bash
# Stage 5.3: install the agentgrid control plane on a clean Linux host.
#
#   ./install-control-plane.sh [--listen 127.0.0.1:7800] [--data-dir /var/lib/agentgrid]
#                             [--ha-role data|witness] [--ha-node-id cp-1]
#                             [--ha-cluster-id prod] [--ha-cluster-token <token>]
#                             [--ha-peers cp-2@url,cp-3@url] [--ha-witnesses w1@url]
#                             [--ha-advertise-url https://host:7800]
#                             [--ha-rpo-mode snapshot|async|sync]
#                             [--ha-snapshot-interval 60] [--ha-allow-plaintext]
#                             [--bin-dir dir]
#
# Creates the unprivileged 'agentgrid' user, data + artifact directories, a
# systemd unit with hardened sandboxing (Stage 5.1), and starts the control
# plane. SQLite lives under DATA_DIR; the unit is the only long-running
# process. Requires systemd.
set -euo pipefail

# Hardening P0: default to loopback only. Pass --listen 0.0.0.0:7800 (with
# TLS configured) only when nodes on other hosts must reach the control plane.
LISTEN="${AGENTGRID_LISTEN:-127.0.0.1:7800}"
BIN_DIR="/usr/local/bin"
DATA_DIR="/var/lib/agentgrid"
DB_DIR="$DATA_DIR/data"
ARTIFACT_DIR="$DATA_DIR/artifacts"
# HA flags (all optional; unset = single-node mode, unchanged behavior).
HA_ROLE=""; HA_NODE_ID=""; HA_CLUSTER_ID=""; HA_CLUSTER_TOKEN=""
HA_PEERS=""; HA_WITNESSES=""; HA_ADVERTISE_URL=""
HA_RPO_MODE="snapshot"; HA_SNAPSHOT_INTERVAL="60"; HA_ALLOW_PLAINTEXT=""
# No baked-in bootstrap credentials: on first run the control plane prints a
# one-time setup token to stdout (journalctl) the operator presents at
# POST /v1/auth/setup to create the first admin. See README.

usage() { echo "usage: $0 [--listen addr] [--data-dir dir] [--ha-role data|witness]
  [--ha-node-id id] [--ha-cluster-id id] [--ha-cluster-token token]
  [--ha-peers peers] [--ha-witnesses witnesses] [--ha-advertise-url url]
  [--ha-rpo-mode snapshot|async|sync] [--ha-snapshot-interval secs]
  [--ha-allow-plaintext] [--bin-dir dir]"; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --listen)            LISTEN="$2";             shift 2;;
    --data-dir)          DATA_DIR="$2"; shift 2
                         DB_DIR="$DATA_DIR/data"; ARTIFACT_DIR="$DATA_DIR/artifacts";;
    --ha-role)           HA_ROLE="$2";            shift 2;;
    --ha-node-id)        HA_NODE_ID="$2";         shift 2;;
    --ha-cluster-id)     HA_CLUSTER_ID="$2";      shift 2;;
    --ha-cluster-token)  HA_CLUSTER_TOKEN="$2";   shift 2;;
    --ha-peers)          HA_PEERS="$2";           shift 2;;
    --ha-witnesses)      HA_WITNESSES="$2";       shift 2;;
    --ha-advertise-url)  HA_ADVERTISE_URL="$2";   shift 2;;
    --ha-rpo-mode)       HA_RPO_MODE="$2";        shift 2;;
    --ha-snapshot-interval) HA_SNAPSHOT_INTERVAL="$2"; shift 2;;
    --ha-allow-plaintext) HA_ALLOW_PLAINTEXT=1;   shift 1;;
    --bin-dir)           BIN_DIR="$2";            shift 2;;
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
# The control plane reads AGENTGRID_DB (default is a RELATIVE
# control-plane.db - under systemd cwd=/ that is an EACCES on /).
Environment=AGENTGRID_DB=$DB_DIR/control-plane.db
Environment=AGENTGRID_ARTIFACT_ROOT=$ARTIFACT_DIR
# AGENTGRID_JWT_SECRET intentionally NOT set here: set it yourself via
# `systemctl edit agentgrid-control-plane` (>=32 bytes) before exposing the
# port. Without it the control plane generates a random per-run secret
# (sessions invalidated on restart) and refuses to start in production mode.
# HA Environment= lines (if any) are appended below.
# Hardening (Stage 5.1): no new privileges, read-only root except data dirs.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=$DATA_DIR
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

# HA flags (emitted only when set; unset = single-node mode, unchanged
# behavior). Environment line order does not matter to systemd.
if [ -n "$HA_ROLE" ]; then
  cat >> /etc/systemd/system/agentgrid-control-plane.service <<EOF
Environment=AGENTGRID_HA_MODE=ha
Environment=AGENTGRID_HA_ROLE=$HA_ROLE
Environment=AGENTGRID_HA_NODE_ID=$HA_NODE_ID
Environment=AGENTGRID_HA_CLUSTER_ID=$HA_CLUSTER_ID
Environment=AGENTGRID_HA_CLUSTER_TOKEN=$HA_CLUSTER_TOKEN
Environment=AGENTGRID_HA_ADVERTISE_URL=$HA_ADVERTISE_URL
Environment=AGENTGRID_HA_PEERS=$HA_PEERS
Environment=AGENTGRID_HA_WITNESSES=$HA_WITNESSES
Environment=AGENTGRID_HA_RPO_MODE=$HA_RPO_MODE
Environment=AGENTGRID_HA_SNAPSHOT_INTERVAL_SECS=$HA_SNAPSHOT_INTERVAL
EOF
fi
[ -n "$HA_ALLOW_PLAINTEXT" ] && {
  cat >> /etc/systemd/system/agentgrid-control-plane.service <<EOF
Environment=AGENTGRID_HA_ALLOW_PLAINTEXT=1
EOF
}

echo ">> enabling + starting"
systemctl daemon-reload
systemctl enable --now agentgrid-control-plane.service
echo ">> control plane listening on $LISTEN. journalctl -u agentgrid-control-plane -f"
echo ">> first run: read the one-time setup token from the logs above,"
echo ">> then: curl -X POST $LISTEN/v1/auth/setup -H 'content-type: application/json'"
echo ">>   -d '{\"setup_token\":\"<token>\",\"username\":\"admin\",\"password\":\"<your>\"}'"
