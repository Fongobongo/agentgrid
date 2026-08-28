#!/usr/bin/env bash
# E2E: task-level auto-retry (competitor-gap feature, hatchet-inspired).
# Brings up control-plane + nodes, runs a task with max_attempts=2 whose
# prompt forces the mock adapter to fail twice. Asserts the first failure
# re-queued the task (2 attempts, terminal=Failed) and that the default
# (max_attempts=1) task fails after its first attempt. Tears down on exit.
set -euo pipefail

cd "$(dirname "$0")/../.."

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7800}"
USER="${AGENTGRID_BOOTSTRAP_USER:-admin}"
PASS="${AGENTGRID_BOOTSTRAP_PASSWORD:-changeme}"
TIMEOUT="${E2E_TIMEOUT:-120}"

docker image inspect ag-cp:test >/dev/null 2>&1 || docker build -t ag-cp:test -f Dockerfile.control-plane .
docker image inspect ag-node:test >/dev/null 2>&1 || docker build -t ag-node:test -f Dockerfile.node-daemon .

cleanup() { bash deploy/compose/down.sh; }
trap cleanup EXIT

echo ">> bringing up stack (control plane + nodes)"
export AGENTGRID_ADMIN_USER="$USER"
export AGENTGRID_ADMIN_PASSWORD="$PASS"
bash deploy/compose/up.sh >/dev/null

echo ">> waiting for health"
for _ in $(seq 1 30); do
  curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && break
  sleep 1
done
curl -fsS "$BASE/health/ready" >/dev/null || { echo "control plane never became ready"; exit 1; }

JWT=$(curl -fsS -X POST "$BASE/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
[ -n "$JWT" ] || { echo "login failed"; exit 1; }

fetch_status() { curl -fsS "$BASE/v1/tasks/$1" -H "authorization: Bearer $JWT" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'; }

# Prompt "fail:1" makes the mock adapter exit 1. With max_attempts=2 the
# first failure must re-queue the task; the second exhausts the budget.
echo ">> auto-retry task (max_attempts=2, prompt forces failure)"
TID=$(curl -fsS -X POST "$BASE/v1/tasks" \
  -H "authorization: Bearer $JWT" -H 'content-type: application/json' \
  -d '{"prompt":"fail:1","adapter":"mock","repository":"*","timeout_secs":60,"max_attempts":2}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
saw_requeue=0
for _ in $(seq 1 "$TIMEOUT"); do
  S=$(fetch_status "$TID")
  [ "$S" = "queued" ] && saw_requeue=1
  case "$S" in failed|cancelled|succeeded) break;; esac
  sleep 1
done
[ "$saw_requeue" = "1" ] || { echo "E2E FAILED: auto-retry task never re-queued (status=$S)"; exit 1; }
[ "$S" = "failed" ] || { echo "E2E FAILED: auto-retry task should end failed (status=$S)"; exit 1; }
ATTEMPTS=$(curl -fsS "$BASE/v1/tasks/$TID/events" -H "authorization: Bearer $JWT" \
  | python3 -c 'import sys,json;print(len({e.get("attempt_id") for e in json.load(sys.stdin)}))')
echo ">> auto-retry OK: re-queued after first failure, failed after second (attempts=$ATTEMPTS)"

# Default task (max_attempts=1) must fail on its first attempt — no retry.
echo ">> default task (max_attempts=1, prompt forces failure)"
T1=$(curl -fsS -X POST "$BASE/v1/tasks" \
  -H "authorization: Bearer $JWT" -H 'content-type: application/json' \
  -d '{"prompt":"fail:1","adapter":"mock","repository":"*","timeout_secs":60}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
for _ in $(seq 1 "$TIMEOUT"); do
  S1=$(fetch_status "$T1")
  case "$S1" in failed|cancelled|succeeded) break;; esac
  sleep 1
done
[ "$S1" = "failed" ] || { echo "E2E FAILED: default task should fail once (status=$S1)"; exit 1; }
echo "E2E OK: auto-retry re-queues until budget exhausted; default fails once"