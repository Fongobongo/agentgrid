#!/usr/bin/env bash
# Stage 5.3 E2E: bring up the agentgrid stack (control plane + two mock nodes
# via docker compose), submit a task, and assert it reaches `succeeded`.
#
# Expects the `ag-cp:test` and `ag-node:test` images to already exist, or
# builds them from the Dockerfiles. Tears the stack down on exit.
set -euo pipefail

cd "$(dirname "$0")/../.."

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7800}"
USER="${AGENTGRID_BOOTSTRAP_USER:-admin}"
PASS="${AGENTGRID_BOOTSTRAP_PASSWORD:-changeme}"
TIMEOUT="${E2E_TIMEOUT:-120}"

# Build images if missing so the script is self-contained in CI.
docker image inspect ag-cp:test >/dev/null 2>&1 || docker build -t ag-cp:test -f Dockerfile.control-plane .
docker image inspect ag-node:test >/dev/null 2>&1 || docker build -t ag-node:test -f Dockerfile.node-daemon .

cleanup() { bash deploy/compose/down.sh; }
trap cleanup EXIT

echo ">> bringing up stack"
# Hardening P0 #2: up.sh reads AGENTGRID_ADMIN_PASSWORD to run the one-shot
# /v1/auth/setup bootstrap. Pin it to the test creds the script logs in with
# below so login does not break against a random admin password.
export AGENTGRID_ADMIN_USER="$USER"
export AGENTGRID_ADMIN_PASSWORD="$PASS"
bash deploy/compose/up.sh >/dev/null

echo ">> waiting for health"
for _ in $(seq 1 30); do
  if curl -fsS "$BASE/health/ready" >/dev/null 2>&1; then break; fi
  sleep 1
done
curl -fsS "$BASE/health/ready" >/dev/null || { echo "control plane never became ready"; exit 1; }

# Hardening P2 item 31: assert the control-plane ran as a non-root user
# (USER agentgrid in the image). root inside the container is a fail.
cp_user="$(docker compose -f docker-compose.yml exec -T control-plane id -u 2>/dev/null || true)"
[ "$cp_user" = "0" ] && { echo "E2E FAILED: control-plane running as root (uid 0)"; exit 1; }
[ -n "$cp_user" ] && echo "control-plane running as non-root uid $cp_user"

JWT=$(curl -fsS -X POST "$BASE/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
[ -n "$JWT" ] || { echo "login failed"; exit 1; }

echo ">> submitting task"
TID=$(curl -fsS -X POST "$BASE/v1/tasks" \
  -H "authorization: Bearer $JWT" -H 'content-type: application/json' \
  -d '{"prompt":"e2e","adapter":"mock","repository":"*","timeout_secs":60}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

echo ">> polling task $TID (timeout ${TIMEOUT}s)"
status=""
for _ in $(seq 1 "$TIMEOUT"); do
  status=$(curl -fsS "$BASE/v1/tasks/$TID" -H "authorization: Bearer $JWT" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
  case "$status" in
    succeeded|failed|cancelled) break;;
  esac
  sleep 1
done

echo "final status: $status"
[ "$status" = "succeeded" ] || { echo "E2E FAILED: task $TID -> $status"; exit 1; }
echo "E2E OK"
