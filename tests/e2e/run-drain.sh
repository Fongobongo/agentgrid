#!/usr/bin/env bash
# Plan 0.2 item 4.3: graceful drain E2E (process-based, no Docker).
#
# Scenario: submit a long task, drain the node mid-flight, then assert:
#   1. the in-flight task still completes `succeeded` (drain never kills work),
#   2. a task submitted while drained stays `queued` (no new assignments),
#   3. after undrain the queued task is assigned and succeeds.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7814}"
PORT="${AGENTGRID_PORT:-7814}"
USER="admin"
source "$ROOT/tests/e2e/lib-bootstrap.sh"
PASS="changeme"

TMP="$(mktemp -d -t ag-e2e-drain-XXXXXX)"
CP_DB="$TMP/cp.db"
NODE_DATA="$TMP/node"
WORK="$TMP/work"
REPOS="$TMP/repos"
mkdir -p "$NODE_DATA" "$WORK" "$REPOS"

CP_PID=""
NODE_PID=""

cleanup() {
  set +e
  [ -n "$NODE_PID" ] && kill -9 "$NODE_PID" 2>/dev/null
  [ -n "$CP_PID" ] && kill -9 "$CP_PID" 2>/dev/null
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

AGENTGRID_LISTEN="127.0.0.1:$PORT" \
AGENTGRID_DB="$CP_DB" \
AGENTGRID_JWT_SECRET="e2e-stable-secret" \
AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
CP_PID=$!

for _ in $(seq 1 40); do
  curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fsS "$BASE/health/ready" >/dev/null || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"

jwt=$(curl -fsS -X POST "$BASE/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
[ -n "$jwt" ] || { echo "login failed"; cat "$TMP/cp.log"; exit 1; }

ENROLL_TOKEN=$(curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
  -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
env PATH="$BIN:$PATH" \
  AGENTGRID_SERVER="$BASE" \
  AGENTGRID_DATA_DIR="$NODE_DATA" \
  AGENTGRID_NODE_NAME="e2e-drain-node" \
  AGENTGRID_WORKSPACE_ROOT="$WORK" \
  AGENTGRID_REPOSITORY_ROOT="$REPOS" \
  AGENTGRID_ADAPTERS="mock" \
  AGENTGRID_MAX_CONCURRENCY="1" \
  RUST_LOG="info" \
  AGENTGRID_ENROLL_TOKEN="$ENROLL_TOKEN" \
  nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
NODE_PID=$!

for _ in $(seq 1 60); do
  st=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st="none"
  [ "$st" = "online" ] && break
  sleep 0.5
done
[ "$st" = "online" ] || { echo "node never came online"; cat "$TMP/node.log"; exit 1; }

NODE_ID=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["id"])')

submit() {
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":\"$1\",\"repository\":\"*\",\"adapter\":\"mock\",\"timeout_secs\":60}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}
status_of() {
  curl -fsS "$BASE/v1/tasks/$1" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'
}

echo ">> submitting long task sleep:8"
T1=$(submit "sleep:8")
for _ in $(seq 1 15); do
  [ "$(status_of "$T1")" = "running" ] && break
  sleep 1
done
[ "$(status_of "$T1")" = "running" ] || { echo "task never reached running"; cat "$TMP/cp.log"; exit 1; }

echo ">> draining node $NODE_ID mid-flight"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/nodes/$NODE_ID/drain?drain=true" \
  -H "authorization: Bearer $jwt")
[ "$code" = "200" ] || { echo "drain returned $code"; exit 1; }

echo ">> submitting second task while drained (must stay queued)"
T2=$(submit "sleep:1")
sleep 6
S2=$(status_of "$T2")
[ "$S2" = "queued" ] || { echo "DRAIN FAILED: task assigned while drained (status=$S2)"; exit 1; }
echo "   second task still queued while drained"

echo ">> in-flight task must still complete"
S1=""
for _ in $(seq 1 30); do
  S1=$(status_of "$T1")
  [ "$S1" = "succeeded" ] && break
  sleep 1
done
[ "$S1" = "succeeded" ] || { echo "in-flight task ended $S1 under drain"; cat "$TMP/node.log"; exit 1; }
echo "   in-flight task succeeded under drain"

echo ">> undrain; queued task must now run"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/nodes/$NODE_ID/drain?drain=false" \
  -H "authorization: Bearer $jwt")
[ "$code" = "200" ] || { echo "undrain returned $code"; exit 1; }
S2=""
for _ in $(seq 1 30); do
  S2=$(status_of "$T2")
  [ "$S2" = "succeeded" ] && break
  sleep 1
done
[ "$S2" = "succeeded" ] || { echo "queued task ended $S2 after undrain"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1; }
echo "   queued task succeeded after undrain"

echo ">> E2E OK (drain: in-flight finishes, no new work, undrain resumes)"
