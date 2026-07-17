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
bash deploy/compose/up.sh >/dev/null

echo ">> waiting for health"
for _ in $(seq 1 30); do
  if curl -fsS "$BASE/health/ready" >/dev/null 2>&1; then break; fi
  sleep 1
done
curl -fsS "$BASE/health/ready" >/dev/null || { echo "control plane never became ready"; exit 1; }

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
    succeeded|failed|cancelled|timed_out) break;;
  esac
  sleep 1
done

echo "final status: $status"
[ "$status" = "succeeded" ] || { echo "E2E FAILED: task $TID -> $status"; exit 1; }
echo "E2E OK"
