#!/usr/bin/env bash
# E2E (process-based, no Docker): node disk-full fail-closed via outbox spool
# limit.
#
# Plan item: «Переполнение диска на node (spool limit)» (Stage 2.x failure
# injection). When the control plane is unreachable for a long window and the
# node keeps streaming adapter output, the on-disk event outbox would grow
# without bound and fill the host disk. The outbox enforces
# `AGENTGRID_OUTBOX_SPOOL_LIMIT_MB`; once exceeded, `push` returns `SpoolFull`,
# the sink latches `spool_full`, emits a terminal `error` event, and the
# attempt completes with `error_code=spool_full` (fail-closed: no more events
# are buffered, the disk is protected).
#
# This script drives that path end-to-end: hold the CP down while a chatty mock
# task runs against a tiny spool limit (1 MiB); bring the CP back; assert the
# task reached `failed` with `error_code=spool_full` in its events and the
# outbox file never exceeded ~1 MiB.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7813}"
PORT="${AGENTGRID_PORT:-7813}"
USER="admin"
PASS="changeme"
SPOOL_BYTES="${AGENTGRID_OUTBOX_SPOOL_LIMIT_BYTES:-4096}"

TMP="$(mktemp -d -t ag-e2e-diskfull-XXXXXX)"
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
  [ -n "$CP_PID" ] && kill "$CP_PID" 2>/dev/null
  pkill -f "$BIN/agentgrid-control-plane" 2>/dev/null
  pkill -f "$BIN/agentgrid-node-daemon" 2>/dev/null
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

start_cp() {
  AGENTGRID_LISTEN="127.0.0.1:$PORT" \
  AGENTGRID_DB="$CP_DB" \
  AGENTGRID_JWT_SECRET="e2e-stable-secret" \
  AGENTGRID_BOOTSTRAP_USER="$USER" \
  AGENTGRID_BOOTSTRAP_PASSWORD="$PASS" \
  AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
  nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
  CP_PID=$!
}

start_node() {
  local tok="${1:-}"
  local env_args=()
  [ -n "$tok" ] && env_args+=(AGENTGRID_ENROLL_TOKEN="$tok")
  env PATH="$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE" \
    AGENTGRID_DATA_DIR="$NODE_DATA" \
    AGENTGRID_NODE_NAME="e2e-disk" \
    AGENTGRID_WORKSPACE_ROOT="$WORK" \
    AGENTGRID_REPOSITORY_ROOT="$REPOS" \
    AGENTGRID_ADAPTERS="mock" \
    AGENTGRID_MAX_CONCURRENCY="1" \
    AGENTGRID_OUTBOX_SPOOL_LIMIT_BYTES="$SPOOL_BYTES" \
    RUST_LOG="info" \
    "${env_args[@]}" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
  NODE_PID=$!
}

stop_cp() {
  [ -n "$CP_PID" ] || return 0
  kill "$CP_PID" 2>/dev/null || true
  for _ in $(seq 1 10); do
    kill -0 "$CP_PID" 2>/dev/null || { CP_PID=""; return 0; }
    sleep 0.2
  done
  kill -9 "$CP_PID" 2>/dev/null || true
  wait "$CP_PID" 2>/dev/null || true
  CP_PID=""
}

wait_ready() {
  for _ in $(seq 1 40); do
    curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}

login() {
  jwt=$(curl -fsS -X POST "$BASE/v1/auth/login" \
    -H 'content-type: application/json' \
    -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  [ -n "$jwt" ] || { echo "login failed"; cat "$TMP/cp.log"; exit 1; }
}

mint_token() {
  ENROLL_TOKEN=$(curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
    -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  [ -n "$ENROLL_TOKEN" ] || { echo "mint token failed"; exit 1; }
}

wait_node_online() {
  for _ in $(seq 1 60); do
    st=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" 2>/dev/null \
      | python3 -c 'import sys,json;ns=json.load(sys.stdin);print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st="none"
    [ "$st" = "online" ] && return 0
    sleep 0.5
  done
  echo "node never came online; status=$st"; cat "$TMP/node.log"; return 1
}

submit() {  # $1 = prompt; prints task id
  local prompt_json
  prompt_json=$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1")
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$prompt_json,\"repository\":\"*\",\"adapter\":\"mock\",\"timeout_secs\":60}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}

poll_status() {
  STATUS=$(curl -fsS "$BASE/v1/tasks/$1" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
}

wait_terminal() {
  STATUS=""
  for _ in $(seq 1 "$1"); do
    poll_status "$2"
    case "$STATUS" in
      succeeded|failed|cancelled|timed_out|lost) return 0;;
    esac
    sleep 1
  done
  return 1
}

echo ">> node disk-full fail-closed (spool limit ${SPOOL_BYTES} bytes)"
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
login
mint_token
start_node "$ENROLL_TOKEN"
wait_node_online || exit 1

echo ">> submitting chatty task (spam:5000 events → ~150 KiB > 4 KiB limit) while CP is up"
TID=$(submit "spam:5000")
echo "  task $TID; waiting 1s for assignment"
sleep 1

echo ">> stopping CP so the node can't flush → outbox grows toward the limit"
stop_cp
echo "  CP down; node streams into the outbox; waiting 8s for spool_full latch (4 KiB limit ≈ 80 lines)"
sleep 8

echo ">> restarting CP (node redelivers the spool_full completion)"
start_cp
wait_ready || { echo "CP not ready after restart"; cat "$TMP/cp.log"; exit 1; }
login

echo ">> polling task for terminal status (expect failed; timeout 30s)"
if wait_terminal 30 "$TID"; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "failed" ] || { echo "  FAILED: expected failed (spool_full), got $STATUS"; cat "$TMP/node.log"; exit 1; }

echo ">> asserting a spool_full error event was emitted"
EVS=$(curl -fsS "$BASE/v1/tasks/$TID/events?after_sequence=0" -H "authorization: Bearer $jwt")
python3 <<PYEOF
import json
evs = json.loads('''$EVS''') if '''$EVS'''.strip() else []
spool = [e for e in evs if e.get("payload",{}).get("event") == "spool_full" or e.get("payload",{}).get("error_code") == "spool_full"]
if not spool:
    print("  FAILED: no spool_full event found"); raise SystemExit(1)
print("  spool_full event present (seq={})".format(spool[0]["sequence"]))
PYEOF

echo ">> asserting the outbox file never grew far past the ${SPOOL_BYTES}-byte limit"
# The file may overshoot by one event line, but must stay bounded (well under
# 2x the limit). Find the attempt's outbox file.
OB_FILE=$(find "$NODE_DATA/outbox" -name '*.jsonl' ! -name 'completions.jsonl' -printf '%p %s\n' 2>/dev/null | sort -k2 -rn | head -1 | cut -d' ' -f1)
if [ -z "$OB_FILE" ]; then
  echo "  (no attempt outbox file found — completion was acked and trimmed; ok)"
else
  SIZE=$(stat -c %s "$OB_FILE")
  CEILING=$((SPOOL_BYTES * 2))
  echo "  outbox file $OB_FILE = ${SIZE} bytes (limit ${SPOOL_BYTES}, ceiling ${CEILING})"
  [ "$SIZE" -le "$CEILING" ] || { echo "  FAILED: outbox exceeded 2x limit"; exit 1; }
fi

echo ">> disk-full fail-closed OK: attempt failed with spool_full, disk bounded"
