#!/usr/bin/env bash
# Stage 5.3 helper: bring up the agentgrid stack with one command.
# Bootstraps the first user, mints two enrollment tokens, writes them to
# .env, then starts the two node-daemon containers.
set -euo pipefail

cd "$(dirname "$0")/../.."

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7800}"
USER="${AGENTGRID_BOOTSTRAP_USER:-admin}"
PASS="${AGENTGRID_BOOTSTRAP_PASSWORD:-changeme}"

echo ">> building & starting control plane"
docker compose up -d control-plane

echo ">> waiting for control plane health"
for _ in $(seq 1 30); do
  if curl -fsS "$BASE/health/ready" >/dev/null 2>&1; then break; fi
  sleep 1
done
curl -fsS "$BASE/health/ready" >/dev/null || { echo "control plane not ready"; exit 1; }

echo ">> logging in as bootstrap user"
JWT=$(curl -fsS -X POST "$BASE/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
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

# Compose auto-loads .env; node services read NODE1_TOKEN/NODE2_TOKEN.
cat > deploy/compose/.env <<EOF
AGENTGRID_BOOTSTRAP_USER=$USER
AGENTGRID_BOOTSTRAP_PASSWORD=$PASS
NODE1_TOKEN=$NODE1_TOKEN
NODE2_TOKEN=$NODE2_TOKEN
EOF

echo ">> starting nodes"
docker compose --env-file deploy/compose/.env up -d node-1 node-2

echo ">> done. control plane: $BASE  (login: $USER / $PASS)"
