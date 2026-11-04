#!/usr/bin/env bash
# Bring up the agentgrid stack with one command. Generates a random JWT
# secret + admin password, starts the control plane, reads the one-time setup
# token from its logs, completes first-user bootstrap via POST /v1/auth/setup,
# mints enrollment tokens for both nodes, writes them to the repo-root .env,
# and starts the node-daemon containers. The bootstrap credentials are printed
# once and are deliberately never written to disk.
#
# Production defaults (no insecure baked-in values); see up --demo for local
# hacking with docker-compose.demo.yml.
set -euo pipefail

cd "$(dirname "$0")/../.."

DEMO=0
[ "${1:-}" = "--demo" ] && DEMO=1

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7800}"

# Compose auto-loads `.env` from the project directory — the repo root, where
# docker-compose.yml lives — and only that file. A `.env` under deploy/compose/
# was never read: a later `docker compose up/ps/down` from a fresh shell lost
# up.sh's process exports and failed the `:?` guards. The demo keeps its own
# --env-file so starting a demo stack cannot clobber production secrets.
if [ "$DEMO" = "1" ]; then
  ENV_FILE=".env.demo"
  COMPOSE=(docker compose --env-file "$ENV_FILE" -f docker-compose.demo.yml)
else
  ENV_FILE=".env"
  COMPOSE=(docker compose -f docker-compose.yml)
fi

# Generate fresh secrets unless an operator pre-set them via env.
# (Finite input through base64 avoids the SIGPIPE you get piping infinite
# /dev/urandom into `head -c` under `set -o pipefail`.)
rand() { head -c 256 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | cut -c1-"$1"; }
ADMIN_USER="${AGENTGRID_ADMIN_USER:-admin}"
ADMIN_PASS="${AGENTGRID_ADMIN_PASSWORD:-$(rand 24)}"
JWT_SECRET="${AGENTGRID_JWT_SECRET:-$(rand 48)}"
# Export as well: process env wins over the .env file, and the demo path uses
# a separate --env-file. Both paths then interpolate the same real values.
export AGENTGRID_JWT_SECRET="$JWT_SECRET"
export NODE1_TOKEN="${NODE1_TOKEN:-bootstrap}"
export NODE2_TOKEN="${NODE2_TOKEN:-bootstrap}"
# Compose auto-loads this repo-root .env on every later command (process env
# wins while up.sh is running). Persist the metrics token too: otherwise a
# restart from a fresh shell silently closes `/metrics` again.
cat > "$ENV_FILE" <<EOF
AGENTGRID_JWT_SECRET=$JWT_SECRET
AGENTGRID_METRICS_TOKEN=${AGENTGRID_METRICS_TOKEN:-}
NODE1_TOKEN=$NODE1_TOKEN
NODE2_TOKEN=$NODE2_TOKEN
EOF
chmod 600 "$ENV_FILE"
echo ">> building & starting control plane"
"${COMPOSE[@]}" up -d control-plane

echo ">> waiting for control plane health"
for _ in $(seq 1 30); do
  if curl -fsS "$BASE/health/ready" >/dev/null 2>&1; then break; fi
  sleep 1
done
curl -fsS "$BASE/health/ready" >/dev/null || { echo "control plane not ready"; exit 1; }

# Read the one-time setup token the control plane printed to stdout at boot.
# (Only minted when no users exist yet; idempotent on subsequent runs.)
# The token sits on the line right after the "=== agentgrid setup token"
# banner; strip the compose log service prefix before use.
SETUP_TOKEN=$("${COMPOSE[@]}" logs control-plane 2>/dev/null \
  | grep -A1 "agentgrid setup token" \
  | awk 'NR==2' \
  | sed 's/^[^|]*| *//' \
  | tr -d '[:space:]' || true)
if [ -n "${SETUP_TOKEN:-}" ]; then
  echo ">> completing first-user bootstrap (POST /v1/auth/setup)"
  curl -fsS -X POST "$BASE/v1/auth/setup" \
    -H 'content-type: application/json' \
    -d "{\"setup_token\":\"$SETUP_TOKEN\",\"username\":\"$ADMIN_USER\",\"password\":\"$ADMIN_PASS\"}" \
    >/dev/null
fi

echo ">> logging in as $ADMIN_USER"
JWT=$(curl -fsS -X POST "$BASE/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$ADMIN_USER\",\"password\":\"$ADMIN_PASS\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
[ -n "$JWT" ] || { echo "login failed"; exit 1; }

mint() {
  # Name the host at mint time. If the container never enrolls — which is what
  # the wait loop below is checking for — the install row is the only record
  # that anyone tried, and it survives a restart of the control plane. Without
  # the body the token goes out and the panel has nothing to show.
  curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
    -H "authorization: Bearer $JWT" \
    -H 'content-type: application/json' \
    -d "{\"host\":\"$1\",\"name\":\"$1\"}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])'
}

echo ">> minting enrollment tokens"
NODE1_TOKEN=$(mint node-1)
NODE2_TOKEN=$(mint node-2)
# Keep the process exports in sync with the file: process env wins over .env,
# and the node services' `:?` guards must see the freshly minted tokens.
export NODE1_TOKEN NODE2_TOKEN

# Real enrollment tokens, written after minting. Compose interpolates from the
# process env here; the file is what a fresh shell will read later.
cat > "$ENV_FILE" <<EOF
AGENTGRID_JWT_SECRET=$JWT_SECRET
AGENTGRID_METRICS_TOKEN=${AGENTGRID_METRICS_TOKEN:-}
NODE1_TOKEN=$NODE1_TOKEN
NODE2_TOKEN=$NODE2_TOKEN
EOF
chmod 600 "$ENV_FILE"

echo ">> starting nodes"
"${COMPOSE[@]}" up -d node-1 node-2

# Wait for both nodes to enroll and heartbeated before stripping tokens, so a
# container that restarts before persisting its credential can still enroll.
echo ">> waiting for nodes to enroll"
for _ in $(seq 1 30); do
  NODES_UP=$(curl -fsS -H "authorization: Bearer $JWT" "$BASE/v1/nodes" \
    | python3 -c 'import sys,json; d=json.load(sys.stdin); ns=d.get("items",d) if isinstance(d,dict) else d; print(sum(1 for n in ns if n.get("status")=="online"))' 2>/dev/null || echo 0)
  [ "${NODES_UP:-0}" -ge 2 ] && break
  sleep 1
done

# Hardening P0 item 6/29: the enrollment tokens are one-time and now consumed
# by the running nodes. Strip them from the env file so the secret does not sit
# on disk after bootstrap. The nodes reuse their persisted credential.json on
# restart, so they do not need the token again.
# Keep non-secret placeholders: `:?` guards interpolate the whole file even for
# `docker compose ps`/`down`, and an empty value would break every command run
# from a fresh shell (up.sh's exports die with this script). The JWT secret and
# the metrics token stay in the 0600, gitignored file so a restart does not
# silently close `/metrics`.
cat > "$ENV_FILE" <<EOF
AGENTGRID_JWT_SECRET=$JWT_SECRET
AGENTGRID_METRICS_TOKEN=${AGENTGRID_METRICS_TOKEN:-}
NODE1_TOKEN=consumed
NODE2_TOKEN=consumed
EOF
chmod 600 "$ENV_FILE"


echo ">> done. control plane: $BASE"
echo ">> login: $ADMIN_USER / $ADMIN_PASS (shown this once)"
