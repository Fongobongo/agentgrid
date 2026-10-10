#!/usr/bin/env bash
# E2E (process-based, no Docker): the Secure-profile gate (Stage 12).
#
# A task with `security_profile: "default-strict"` (name ends in `-strict`)
# must NOT assign to a wrapper-only node — the scheduler reports
# "requires structured permission interception" — and MUST assign to an
# ACP/structured node, where it runs to `succeeded`. This is the executable
# half of the Secure profile (ADR 0015): strict tasks only land where tool
# calls are intercepted.
#
# Nodes: n1 = mock (wrapper), n2 = fake-acp:acp (structured; test ACP agent
# built with the workspace). Task adapter is fake-acp so both nodes are
# adapter-eligible and the ONLY discriminator is the strict gate.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

[ -x "$BIN/adapter-fake-acp" ] || { echo "build debug binaries first (adapter-fake-acp missing)"; exit 1; }

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7817}"
PORT="${AGENTGRID_PORT:-7817}"
USER="admin"
PASS="changeme"
source "$ROOT/tests/e2e/lib-bootstrap.sh"

TMP="$(mktemp -d -t ag-e2e-secure-XXXXXX)"
CP_DB="$TMP/cp.db"
mkdir -p "$TMP/n1" "$TMP/w1" "$TMP/r1" "$TMP/n2" "$TMP/w2" "$TMP/r2"

CP_PID=""
N1_PID=""
N2_PID=""

cleanup() {
  set +e
  [ -n "$N2_PID" ] && kill -9 "$N2_PID" 2>/dev/null
  [ -n "$N1_PID" ] && kill -9 "$N1_PID" 2>/dev/null
  [ -n "$CP_PID" ] && kill "$CP_PID" 2>/dev/null
  pkill -f "$BIN/agentgrid-control-plane" 2>/dev/null
  pkill -f "$BIN/agentgrid-node-daemon" 2>/dev/null
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

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

start_cp() {
  AGENTGRID_LISTEN="127.0.0.1:$PORT" \
  AGENTGRID_DB="$CP_DB" \
  AGENTGRID_JWT_SECRET="e2e-secure-secret" \
  AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
  nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
  CP_PID=$!
}

# $1 = name, $2 = adapters, $3 = tag (n1/n2 dirs + log)
start_node() {
  env PATH="$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE" \
    AGENTGRID_DATA_DIR="$TMP/$3" \
    AGENTGRID_NODE_NAME="e2e-secure-$1" \
    AGENTGRID_WORKSPACE_ROOT="$TMP/w$([ "$3" = n1 ] && echo 1 || echo 2)" \
    AGENTGRID_REPOSITORY_ROOT="$TMP/r$([ "$3" = n1 ] && echo 1 || echo 2)" \
    AGENTGRID_ADAPTERS="$2" \
    AGENTGRID_MAX_CONCURRENCY="1" \
    RUST_LOG="info" \
    AGENTGRID_ENROLL_TOKEN="$ENROLL_TOKEN" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/$3.log" 2>&1 &
  if [ "$3" = "n1" ]; then N1_PID=$!; else N2_PID=$!; fi
}

wait_nodes_online() {  # $1 = count
  for _ in $(seq 1 60); do
    n=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" 2>/dev/null \
      | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(sum(1 for x in ns if x.get("status")=="online"))' 2>/dev/null) || n=0
    [ "$n" = "$1" ] && return 0
    sleep 0.5
  done
  echo "wanted $1 online nodes, have $n"; return 1
}

submit_strict() {  # $1 = prompt; prints task id
  local prompt_json
  prompt_json=$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1")
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$prompt_json,\"repository\":\"*\",\"adapter\":\"fake-acp\",\"security_profile\":\"default-strict\",\"timeout_secs\":120}" \
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

echo ">> secure-profile gate: wrapper node first"
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
login
mint_token
start_node wrapper mock n1
wait_nodes_online 1 || exit 1

echo ">> submitting default-strict task (wrapper node must refuse it)"
TID=$(submit_strict "secure e2e")
sleep 8
poll_status "$TID"
[ "$STATUS" = "queued" ] || { echo "  FAILED: strict task left queued on a wrapper node: $STATUS"; exit 1; }
echo "  still queued on wrapper-only fleet, as required"
curl -fsS "$BASE/v1/tasks/$TID/eligibility" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;d=json.load(sys.stdin);rs=[r for n in d.get("nodes",[]) for r in n.get("reasons",[])];assert any("structured" in r for r in rs),"no structured-interception reason: %r"%rs;print("  eligibility reason present:",[r for r in rs if "structured" in r][0])'

echo ">> adding structured (ACP) node: the same task must assign and succeed"
start_node structured "fake-acp:acp" n2
wait_nodes_online 2 || exit 1
wait_terminal 120 "$TID" || { echo "  task never terminal: $STATUS"; cat "$TMP/n2.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: expected succeeded on structured node, got $STATUS"; cat "$TMP/n2.log"; exit 1; }
echo "  strict task succeeded on the structured node"

echo ">> secure-profile gate OK (refused wrapper, ran structured)"
