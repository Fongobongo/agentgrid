#!/usr/bin/env bash
# Shared process-based E2E harness: one control plane + N nodes on 127.0.0.1,
# driven over HTTP with curl + python3. Extracted from the copy-pasted
# boilerplate of run-*.sh so new scenarios stop cloning ~100 lines.
#
# Prerequisites (set by the caller BEFORE sourcing, after its own mktemp):
#   BIN   directory with debug binaries (usually "$ROOT/target/debug")
#   BASE  control-plane base URL, e.g. http://127.0.0.1:7819
#   PORT  control-plane port (matches BASE)
#   TMP   scratch dir (removed by e2e_cleanup unless AG_E2E_KEEP=1)
#   USER/PASS  bootstrap admin credentials
#   ROOT  repo root (for lib-bootstrap.sh)
# Knobs (all optional):
#   CP_DB               CP sqlite path (default $TMP/cp.db)
#   E2E_JWT_SECRET      CP jwt secret (default e2e-secret)
#   E2E_CONCURRENCY     node max concurrency (default 2)
#   ENROLL_TOKEN        set by e2e_mint_token; reused on node restarts
# Results (globals callers read):
#   CP_PID, NODE_PID (last started), E2E_PIDS (all started, for cleanup),
#   jwt, ENROLL_TOKEN, STATUS (last polled task status)
#
# Usage:
#   cd "$(dirname "$0")/../.."; ROOT="$PWD"; BIN="$ROOT/target/debug"
#   BASE="http://127.0.0.1:7819"; PORT=7819; USER=admin; PASS=changeme
#   TMP="$(mktemp -d -t ag-e2e-x-XXXXXX)"; mkdir -p ...
#   source "$ROOT/tests/e2e/lib-e2e.sh"
#   trap e2e_cleanup EXIT
#   e2e_check_bins
#   e2e_start_cp; e2e_wait_ready || exit 1
#   bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
#   e2e_login; e2e_mint_token
#   e2e_start_node n1 "e2e-n1" "mock" "$WORK" "$REPOS" "$NODE_DATA"
#   NODE1_PID=$NODE_PID
#   e2e_wait_nodes_online 1 || exit 1
#   TID=$(e2e_submit "spam:10"); e2e_wait_terminal 60 "$TID"
#
# Scripts with exotic teardown (two-host SSH cleanup, compose stacks) keep
# their own cleanup function; everyone else traps e2e_cleanup.

set -euo pipefail

# shellcheck disable=SC1091
source "$ROOT/tests/e2e/lib-bootstrap.sh"

CP_PID=""
NODE_PID=""
E2E_PIDS=""

e2e_check_bins() {
  for b in agentgrid-control-plane agentgrid-node-daemon adapter-mock; do
    [ -x "$BIN/$b" ] || { echo "build debug binaries first ($b missing)"; exit 1; }
  done
}

e2e_start_cp() {
  local db="${CP_DB:-$TMP/cp.db}" secret="${E2E_JWT_SECRET:-e2e-secret}"
  AGENTGRID_LISTEN="127.0.0.1:$PORT" \
  AGENTGRID_DB="$db" \
  AGENTGRID_JWT_SECRET="$secret" \
  AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
  nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
  CP_PID=$!
  E2E_PIDS="$E2E_PIDS $CP_PID"
}

e2e_wait_ready() {
  for _ in $(seq 1 40); do
    curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}

e2e_login() {
  jwt=$(curl -fsS -X POST "$BASE/v1/auth/login" \
    -H 'content-type: application/json' \
    -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  [ -n "$jwt" ] || { echo "login failed"; cat "$TMP/cp.log"; exit 1; }
}

e2e_mint_token() {
  ENROLL_TOKEN=$(curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
    -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  [ -n "$ENROLL_TOKEN" ] || { echo "mint token failed"; exit 1; }
}

# e2e_start_node <tag> <node-name> <adapters-csv> <work> <repos> <datadir> [VAR=val...]
# Extra VAR=val pairs are appended to the daemon env (sandbox knobs, stub
# bins, ...). Log lands in $TMP/<tag>.log; pid in $NODE_PID (+ E2E_PIDS).
e2e_start_node() {
  local tag="$1" name="$2" adapters="$3" work="$4" repos="$5" datadir="$6"
  shift 6
  env PATH="$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE" \
    AGENTGRID_DATA_DIR="$datadir" \
    AGENTGRID_NODE_NAME="$name" \
    AGENTGRID_WORKSPACE_ROOT="$work" \
    AGENTGRID_REPOSITORY_ROOT="$repos" \
    AGENTGRID_ADAPTERS="$adapters" \
    AGENTGRID_MAX_CONCURRENCY="${E2E_CONCURRENCY:-2}" \
    RUST_LOG="${RUST_LOG:-info}" \
    AGENTGRID_ENROLL_TOKEN="$ENROLL_TOKEN" \
    "$@" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/$tag.log" 2>&1 &
  NODE_PID=$!
  E2E_PIDS="$E2E_PIDS $NODE_PID"
}

e2e_wait_nodes_online() {  # $1 = expected count
  local want="$1" n="0"
  for _ in $(seq 1 60); do
    n=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" 2>/dev/null \
      | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(sum(1 for x in ns if x.get("status")=="online"))' 2>/dev/null) || n="0"
    [ "$n" = "$want" ] && return 0
    sleep 0.5
  done
  echo "wanted $want online nodes, have $n"; return 1
}

# e2e_submit <prompt> [adapter mock] [repository *] [timeout 120]; prints id.
e2e_submit() {
  local prompt_json
  prompt_json=$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1")
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$prompt_json,\"repository\":\"${3:-*}\",\"adapter\":\"${2:-mock}\",\"timeout_secs\":${4:-120}}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}

e2e_poll_status() {  # $1 = task id; sets STATUS
  STATUS=$(curl -fsS "$BASE/v1/tasks/$1" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
}

e2e_wait_terminal() {  # $1 = seconds, $2 = task id; sets STATUS
  STATUS=""
  for _ in $(seq 1 "$1"); do
    e2e_poll_status "$2"
    case "$STATUS" in
      succeeded|failed|cancelled|timed_out|lost) return 0;;
    esac
    sleep 1
  done
  return 1
}

e2e_wait_status() {  # $1 = want, $2 = task id, $3 = seconds
  for _ in $(seq 1 "$3"); do
    e2e_poll_status "$2"
    [ "$STATUS" = "$1" ] && return 0
    sleep 1
  done
  return 1
}

# e2e_task_events <task id>; prints the events JSON array.
e2e_task_events() {
  curl -fsS "$BASE/v1/tasks/$1/events?after_sequence=0" -H "authorization: Bearer $jwt"
}

e2e_cleanup() {
  set +e
  for p in $E2E_PIDS; do kill -9 "$p" 2>/dev/null; done
  pkill -f "$BIN/agentgrid-control-plane" 2>/dev/null
  pkill -f "$BIN/agentgrid-node-daemon" 2>/dev/null
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
