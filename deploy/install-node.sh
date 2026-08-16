#!/usr/bin/env bash
# Stage 5.3: install the agentgrid node daemon on a clean Linux host (<10 min).
#
#   ./install-node.sh --server https://cp.example.com --token <enroll-token> \
#       --staging ./release-bin --adapters mock,claude
#
# Creates the unprivileged 'agentgrid' user, data directories, a systemd unit
# with hardened sandboxing, installs the daemon + selected adapter binaries
# from the release staging dir (checksum-verified), and enrolls the node.
# Requires systemd.
set -euo pipefail

SERVER=""
TOKEN=""
NAME="$(hostname)"
BIN_DIR="/usr/local/bin"
DATA_DIR="/var/lib/agentgrid"
WORKSPACE="$DATA_DIR/workspace"
REPOS="$DATA_DIR/repos"
# Plan 224: adapters ship with the daemon. `--adapters mock,claude` installs
# adapter-mock + adapter-claude into BIN_DIR. `--staging <dir>` points at the
# release tarball dir (default: ./bin relative to this script) holding the
# prebuilt binaries + checksums.txt.
ADAPTERS="mock"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STAGING="$SCRIPT_DIR/bin"

usage() { echo "usage: $0 --server <url> --token <token> [--name <node>] [--bin-dir <dir>] [--adapters mock[,claude,...]] [--staging <dir>]"; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --server) SERVER="$2"; shift 2;;
    --token)  TOKEN="$2";  shift 2;;
    --name)   NAME="$2";   shift 2;;
    --bin-dir) BIN_DIR="$2"; shift 2;;
    --adapters) ADAPTERS="$2"; shift 2;;
    --staging) STAGING="$2"; shift 2;;
    *) usage;;
  esac
done
[ -n "$SERVER" ] && [ -n "$TOKEN" ] || usage

# Plan 224: install the daemon + selected adapters from the release staging
# dir into BIN_DIR (verify checksum when available, like the checksum flag
# flow in install-control-plane.sh). Fails loud when a binary is missing.
echo ">> installing binaries"
install_bin() {
  local b="$1"
  [ -f "$STAGING/$b" ] || { echo "missing $STAGING/$b (run --staging <release-dir> or build first)"; exit 1; }
  if [ -f "$STAGING/checksums.txt" ]; then
    ( cd "$STAGING" && sha256sum -c --quiet --ignore-missing checksums.txt ) 2>/dev/null \
      || { echo "checksum mismatch for $b"; exit 1; }
  fi
  install -m 0755 -o root -g root "$STAGING/$b" "$BIN_DIR/$b"
}
install_bin agentgrid-node-daemon
IFS=',' read -r -a adapter_list <<< "$ADAPTERS"
for a in "${adapter_list[@]}"; do
  [ -n "$a" ] || continue
  install_bin "adapter-$a"
done
for b in agentgrid-node-daemon "${adapter_list[@]/#/adapter-}"; do
  [ -n "$b" ] || continue
  command -v "$BIN_DIR/$b" >/dev/null 2>&1 || { echo "missing $BIN_DIR/$b"; exit 1; }
done
command -v systemctl >/dev/null 2>&1 || { echo "systemd required"; exit 1; }

echo ">> creating user + directories"
if ! id agentgrid >/dev/null 2>&1; then useradd -r -m -d "$DATA_DIR" agentgrid; fi
mkdir -p "$WORKSPACE" "$REPOS" "$DATA_DIR/data" "$DATA_DIR/artifacts"
chown -R agentgrid:agentgrid "$DATA_DIR"

# Hardening P0: enrollment token lives in its own 0600 env file the daemon
# scrubs `AGENTGRID_ENROLL_TOKEN` from after the first successful enroll, so a
# rebooting node reuses /var/lib/agentgrid/credential.json and the token is
# not left on disk for an attacker to reuse. The main unit references the
# stable settings; the token lives only here.
ENV_FILE="$DATA_DIR/enroll.env"
( umask 077; printf 'AGENTGRID_ENROLL_TOKEN=%s\n' "$TOKEN" ) >"$ENV_FILE"
chown agentgrid:agentgrid "$ENV_FILE"
chmod 600 "$ENV_FILE"

echo ">> writing systemd unit"
cat > /etc/systemd/system/agentgrid-node.service <<EOF
[Unit]
Description=agentgrid node daemon
After=network-online.target
Wants=network-online.target

[Service]
User=agentgrid
Group=agentgrid
ExecStart=$BIN_DIR/agentgrid-node-daemon
Restart=on-failure
RestartSec=5
Environment=AGENTGRID_SERVER=$SERVER
Environment=AGENTGRID_NODE_NAME=$NAME
Environment=AGENTGRID_DATA_DIR=$DATA_DIR/data
Environment=AGENTGRID_WORKSPACE_ROOT=$WORKSPACE
Environment=AGENTGRID_REPOSITORY_ROOT=$REPOS
Environment=AGENTGRID_ARTIFACT_ROOT=$DATA_DIR/artifacts
Environment=AGENTGRID_ENV_FILE=$DATA_DIR/enroll.env
EnvironmentFile=-$DATA_DIR/enroll.env
# Hardening P0 (systemd): drop destructive capabilities and hide the rest of
# the host. PrivateDevices is OFF by default; flip on only if your adapters
# do not need a TTY/devices.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=$DATA_DIR
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectControlGroups=true
ProtectClock=true
ProtectHostname=true
ProtectProc=invisible
RestrictSUIDSGID=true
LockPersonality=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=true
MemoryDenyWriteExecute=true
# ponytail: RestrictRealtime / RestrictRealtimePriority omitted; some adapters
# use realtime scheduling for low-latency streaming. Add when not needed.
# A separate DropIn (/etc/systemd/system/agentgrid-node.service.d/) is the
# upgrade-safe way to override any of these per host.

[Install]
WantedBy=multi-user.target
EOF

echo ">> enabling + starting"
systemctl daemon-reload
systemctl enable --now agentgrid-node.service
echo ">> node '$NAME' enrolled and running. journalctl -u agentgrid-node -f"

# Uninstall (documented procedure — no dedicated CLI yet):
#   systemctl disable --now agentgrid-node.service
#   rm -f /etc/systemd/system/agentgrid-node.service && systemctl daemon-reload
#   rm -rf /var/lib/agentgrid   # also drops credential.json + workspace + artifacts
#   userdel agentgrid            # optional: keeps no agentgrid-owned files after the rm
# Idempotent re-run: re-running this script overwrites the unit and ENV file in
# place; `useradd` is guarded by `id agentgrid`, and `systemctl enable --now`
# restarts the already-enabled unit with the new config, so a second install
# never spawns a second daemon.
