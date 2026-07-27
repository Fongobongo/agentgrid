#!/usr/bin/env bash
# Bring up the agentgrid stack with one command. Generates a random JWT
# secret + admin password, starts the control plane, reads the one-time setup
# token from its logs, completes first-user bootstrap via POST /v1/auth/setup,
# mints enrollment tokens for both nodes, writes them to .env, and starts the
# node-daemon containers. The bootstrap credentials are printed once.
#
# Production defaults (no insecure baked-in values); see up --demo for local
# hacking with docker-compose.demo.yml.
set -euo pipefail

cd "$(dirname "$0")/../.."

DEMO=0
[ "${1:-}" = "--demo" ] && DEMO=1

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7800}"

if [ "$DEMO" = "1" ]; then
  COMPOSE=(docker compose -f docker-compose.demo.yml)
  ENV_FILE="deploy/compose/.env.demo"
else
  COMPOSE=(docker compose -f docker-compose.yml)
  ENV_FILE="deploy/compose/.env"
fi

# Generate fresh secrets unless an operator pre-set them via env.
ADMIN_USER="${AGENTGRID_ADMIN_USER:-admin}"
ADMIN_PASS="${AGENTGRID_ADMIN_PASSWORD:-$(LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c 24)}"
JWT_SECRET="${AGENTGRID_JWT_SECRET:-$(LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c 48)}"
export AGENTGRID_JWT_SECRET

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
SETUP_TOKEN=$(grep -m1 "agentgrid setup token" "${COMPOSE[@]}" logs control-plane 2>/dev/null \
  | sed -n '2p' || true)
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
  curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
    -H "authorization: Bearer $JWT" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])'
}

echo ">> minting enrollment tokens"
NODE1_TOKEN=$(mint)
NODE2_TOKEN=$(mint)

# Compose auto-loads .env alongside the compose file; node services read it.
cat > "$ENV_FILE" <<EOF
AGENTGRID_JWT_SECRET=$JWT_SECRET
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
    | python3 -c 'import sys,json; d=json.load(sys.stdin); print(sum(1 for n in d if n.get("status")=="online"))' 2>/dev/null || echo 0)
  [ "${NODES_UP:-0}" -ge 2 ] && break
  sleep 1
done

# Hardening P0 item 6/29: the enrollment tokens are one-time and now consumed
# by the running nodes. Strip them from the env file so the secret does not sit
# on disk after bootstrap. The nodes reuse their persisted credential.json on
# restart, so they do not need the token again.
cat > "$ENV_FILE" <<EOF
AGENTGRID_JWT_SECRET=$JWT_SECRET
EOF
chmod 600 "$ENV_FILE"


echo ">> done. control plane: $BASE"
echo ">> login: $ADMIN_USER / $ADMIN_PASS (shown this once)"
