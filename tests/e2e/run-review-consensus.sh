#!/usr/bin/env bash
# E2E: consensus patch review (competitor-gap feature, nitpicker-inspired).
# Brings up control-plane + nodes, runs one target task to success (pending
# patch review), fires two review tasks (consensus_mode=review, review_of=
# target) whose prompts end in APPROVE — the mock adapter echoes the last
# prompt line as its result text, so both verdicts come back APPROVE and the
# CP auto-approves the target's patch review. Tears the stack down on exit.
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

echo ">> target task (succeeds -> pending patch review)"
TARGET=$(curl -fsS -X POST "$BASE/v1/tasks" \
  -H "authorization: Bearer $JWT" -H 'content-type: application/json' \
  -d '{"prompt":"e2e review target","adapter":"mock","repository":"*","timeout_secs":60}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
for _ in $(seq 1 "$TIMEOUT"); do
  [ "$(fetch_status "$TARGET")" = "succeeded" ] && break
  sleep 1
done
[ "$(fetch_status "$TARGET")" = "succeeded" ] || { echo "E2E FAILED: target $TARGET -> $(fetch_status "$TARGET")"; exit 1; }

APPROVAL=$(curl -fsS "$BASE/v1/tasks/$TARGET/review-approval" -H "authorization: Bearer $JWT")
[ "$APPROVAL" != "null" ] || { echo "E2E FAILED: no pending patch review for $TARGET"; exit 1; }
echo ">> pending patch review present"

# Two reviewers, mock echoes the last prompt line (APPROVE) as the result.
GROUP=$(python3 -c 'import uuid;print(uuid.uuid4())')
R1=""
R2=""
for M in mock mock; do
  ID=$(curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $JWT" -H 'content-type: application/json' \
    -d "{\"prompt\":\"APPROVE\",\"adapter\":\"$M\",\"repository\":\"*\",\"timeout_secs\":60,\"consensus_group_id\":\"$GROUP\",\"consensus_member\":\"$M\",\"consensus_mode\":\"review\",\"review_of\":\"$TARGET\"}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
  echo ">> reviewer $M -> $ID"
  if [ -z "$R1" ]; then R1="$ID"; else R2="$ID"; fi
done

echo ">> waiting for reviewers + auto-approval"
approved=0
for _ in $(seq 1 "$TIMEOUT"); do
  S1=$(fetch_status "$R1"); S2=$(fetch_status "$R2")
  APPROVAL=$(curl -fsS "$BASE/v1/tasks/$TARGET/review-approval" -H "authorization: Bearer $JWT")
  if [ "$S1" = "succeeded" ] && [ "$S2" = "succeeded" ] && [ "$APPROVAL" = "null" ]; then
    approved=1; break
  fi
  sleep 1
done
[ "$approved" = "1" ] || {
  echo "E2E FAILED: reviewers=$S1/$S2 approval=$APPROVAL"
  echo ">> reviewer result events:"
  for R in "$R1" "$R2"; do
    echo "-- task $R"
    curl -fsS "$BASE/v1/tasks/$R/events" -H "authorization: Bearer $JWT" 2>/dev/null \
      | python3 -c 'import sys,json
try:
  for e in json.load(sys.stdin):
    if isinstance(e,dict) and e.get("type")=="result": print("  result:",e.get("payload"))
except Exception as ex: print("  parse fail",ex)'
  done
  echo ">> audit review_consensus markers:"
  curl -fsS "$BASE/v1/audit" -H "authorization: Bearer $JWT" 2>/dev/null \
    | python3 -c 'import sys,json
try:
  for a in json.load(sys.stdin):
    if "review_consensus" in str(a.get("action","")): print(" ",a)
except Exception as ex: print("  parse fail",ex)'
  exit 1
}
echo "E2E OK: unanimous APPROVE auto-approved the patch review"