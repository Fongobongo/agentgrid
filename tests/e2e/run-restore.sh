#!/usr/bin/env bash
# Plan 0.2 item 4.1: backup/restore rehearsal (process-based, no Docker).
#
# Scenario: bring up CP1 + a node, run one task to succeeded, snapshot via
# POST /v1/admin/backup, tear everything down, start a FRESH control plane on
# a copy of the backup (new port, new data dir), verify the old task is
# visible with its status, enroll a new node, and run one more task through to
# succeeded on the restored instance.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

PORT1="${AGENTGRID_PORT1:-7812}"
PORT2="${AGENTGRID_PORT2:-7813}"
USER="admin"
source "$ROOT/tests/e2e/lib-bootstrap.sh"
PASS="changeme"

TMP="$(mktemp -d -t ag-e2e-restore-XXXXXX)"
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

start_cp() { # $1 = port, $2 = db path, $3 = artifact root, $4 = log name
  AGENTGRID_LISTEN="127.0.0.1:$1" \
  AGENTGRID_DB="$2" \
  AGENTGRID_JWT_SECRET="e2e-stable-secret" \
  AGENTGRID_ARTIFACT_ROOT="$3" \
  nohup "$BIN/agentgrid-control-plane" >"$TMP/$4" 2>&1 &
  CP_PID=$!
}

start_node() { # $1 = base url, $2 = enroll token (may be empty)
  local base="$1" tok="${2:-}"
  local env_args=()
  [ -n "$tok" ] && env_args+=(AGENTGRID_ENROLL_TOKEN="$tok")
  env PATH="$BIN:$PATH" \
    AGENTGRID_SERVER="$base" \
    AGENTGRID_DATA_DIR="$NODE_DATA" \
    AGENTGRID_NODE_NAME="e2e-restore-node" \
    AGENTGRID_WORKSPACE_ROOT="$WORK" \
    AGENTGRID_REPOSITORY_ROOT="$REPOS" \
    AGENTGRID_ADAPTERS="mock" \
    AGENTGRID_MAX_CONCURRENCY="2" \
    RUST_LOG="info" \
    "${env_args[@]}" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
  NODE_PID=$!
}

wait_ready() { # $1 = base url
  for _ in $(seq 1 40); do
    curl -fsS "$1/health/ready" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}

BASE1="http://127.0.0.1:$PORT1"
BASE2="http://127.0.0.1:$PORT2"

echo ">> starting CP1 on $BASE1"
start_cp "$PORT1" "$CP_DB" "$TMP/artifacts" "cp1.log"
wait_ready "$BASE1" || { echo "CP1 not ready"; cat "$TMP/cp1.log"; exit 1; }
bootstrap_first_user "$TMP/cp1.log" "$BASE1" "$USER" "$PASS"

jwt=$(curl -fsS -X POST "$BASE1/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
[ -n "$jwt" ] || { echo "login failed"; cat "$TMP/cp1.log"; exit 1; }

ENROLL_TOKEN=$(curl -fsS -X POST "$BASE1/v1/nodes/enrollment-token" \
  -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
start_node "$BASE1" "$ENROLL_TOKEN"

echo ">> waiting for node online"
for _ in $(seq 1 60); do
  st=$(curl -fsS "$BASE1/v1/nodes" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st="none"
  [ "$st" = "online" ] && break
  sleep 0.5
done
[ "$st" = "online" ] || { echo "node never came online"; cat "$TMP/node.log"; exit 1; }

echo ">> submitting task on CP1"
TID=$(curl -fsS -X POST "$BASE1/v1/tasks" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d '{"prompt":"sleep:2","repository":"*","adapter":"mock","timeout_secs":60}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
STATUS=""
for _ in $(seq 1 60); do
  STATUS=$(curl -fsS "$BASE1/v1/tasks/$TID" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
  [ "$STATUS" = "succeeded" ] && break
  sleep 1
done
[ "$STATUS" = "succeeded" ] || { echo "task on CP1 ended $STATUS"; cat "$TMP/cp1.log"; cat "$TMP/node.log"; exit 1; }
echo "   task $TID succeeded on CP1"

echo ">> taking backup via POST /v1/admin/backup"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE1/v1/admin/backup" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d '{"path":"backup.db"}')
[ "$code" = "200" ] || { echo "backup returned $code"; exit 1; }
[ -s "$TMP/backup.db" ] || { echo "backup file missing/empty"; exit 1; }
echo "   backup written: $(stat -c%s "$TMP/backup.db") bytes"

echo ">> tearing down CP1 + node"
kill -9 "$NODE_PID" 2>/dev/null; wait "$NODE_PID" 2>/dev/null || true; NODE_PID=""
kill -9 "$CP_PID" 2>/dev/null; wait "$CP_PID" 2>/dev/null || true; CP_PID=""

echo ">> starting CP2 (restored instance) on $BASE2"
cp "$TMP/backup.db" "$TMP/restored.db"
rm -rf "$NODE_DATA"; mkdir -p "$NODE_DATA"
start_cp "$PORT2" "$TMP/restored.db" "$TMP/artifacts2" "cp2.log"
wait_ready "$BASE2" || { echo "CP2 not ready"; cat "$TMP/cp2.log"; exit 1; }

echo ">> logging into restored instance (users table came from the backup)"
jwt=$(curl -fsS -X POST "$BASE2/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
[ -n "$jwt" ] || { echo "login on restored CP failed"; cat "$TMP/cp2.log"; exit 1; }

echo ">> asserting pre-backup task is visible with its status"
STATUS=$(curl -fsS "$BASE2/v1/tasks/$TID" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
[ "$STATUS" = "succeeded" ] || { echo "restored task status is $STATUS"; exit 1; }
echo "   task $TID still succeeded after restore"

echo ">> enrolling a fresh node on CP2 and running one task"
ENROLL_TOKEN=$(curl -fsS -X POST "$BASE2/v1/nodes/enrollment-token" \
  -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
start_node "$BASE2" "$ENROLL_TOKEN"
for _ in $(seq 1 60); do
  st=$(curl -fsS "$BASE2/v1/nodes" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st="none"
  [ "$st" = "online" ] && break
  sleep 0.5
done
[ "$st" = "online" ] || { echo "node never came online on CP2"; cat "$TMP/node.log"; exit 1; }

TID2=$(curl -fsS -X POST "$BASE2/v1/tasks" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d '{"prompt":"sleep:2","repository":"*","adapter":"mock","timeout_secs":60}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
STATUS=""
for _ in $(seq 1 60); do
  STATUS=$(curl -fsS "$BASE2/v1/tasks/$TID2" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
  [ "$STATUS" = "succeeded" ] && break
  sleep 1
done
[ "$STATUS" = "succeeded" ] || { echo "task on restored CP ended $STATUS"; cat "$TMP/cp2.log"; cat "$TMP/node.log"; exit 1; }
echo "   task $TID2 succeeded on the restored instance"

echo ">> E2E OK (backup → restore → old state visible → new task runs)"
