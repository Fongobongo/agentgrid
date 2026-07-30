#!/usr/bin/env bash
# Slow-network failure injection (process-based, no Docker / no `tc`).
#
# Routes node→CP traffic through a throttling proxy (throttle-proxy.py) that
# sleeps DELAY_MS before every socket write, widening every round-trip. A
# chatty mock task (spam:200) must still complete with contiguous event
# sequences — no gaps, no duplicates, no spurious timeouts — under the
# inflated latency. This exercises the node's retry/idempotency path under
# slow-network conditions (failure-injection checklist item).
#
# Layout: CP on :7811 (real), proxy on :7820 (slow), node pointed at :7820.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE_REAL="http://127.0.0.1:7811"
BASE_PROXY="http://127.0.0.1:7820"
USER="admin"
PASS="changeme"
# Hardening P0 #2: the bootstrap env backdoor was removed; source the shared
# helper that reads the one-time setup token from the CP log.
source "$ROOT/tests/e2e/lib-bootstrap.sh"

TMP="$(mktemp -d -t ag-e2e-slow-XXXXXX)"
CP_DB="$TMP/cp.db"
NODE_DATA="$TMP/node"
WORK="$TMP/work"
REPOS="$TMP/repos"
mkdir -p "$NODE_DATA" "$WORK" "$REPOS"

CP_PID=""; PROXY_PID=""; NODE_PID=""

cleanup() {
  set +e
  [ -n "$NODE_PID" ] && kill -9 "$NODE_PID" 2>/dev/null
  [ -n "$PROXY_PID" ] && kill "$PROXY_PID" 2>/dev/null
  [ -n "$CP_PID" ] && kill "$CP_PID" 2>/dev/null
  pkill -f "$BIN/agentgrid-control-plane" 2>/dev/null
  pkill -f "$BIN/agentgrid-node-daemon" 2>/dev/null
  pkill -f "throttle-proxy.py" 2>/dev/null
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

start_cp() {
  AGENTGRID_LISTEN="127.0.0.1:7811" \
  AGENTGRID_DB="$CP_DB" \
  AGENTGRID_JWT_SECRET="e2e-stable-secret" \
  AGENTGRID_BOOTSTRAP_USER="$USER" \
  AGENTGRID_BOOTSTRAP_PASSWORD="$PASS" \
  AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
  nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
  CP_PID=$!
}

start_proxy() {
  AG_PROXY_LISTEN="127.0.0.1:7820" \
  AG_PROXY_TARGET="127.0.0.1:7811" \
  AG_PROXY_DELAY_MS="${AG_E2E_SLOW_MS:-250}" \
  nohup python3 "$ROOT/tests/e2e/throttle-proxy.py" >"$TMP/proxy.log" 2>&1 &
  PROXY_PID=$!
}

start_node() {
  local tok="${1:-}"
  local env_args=()
  [ -n "$tok" ] && env_args+=(AGENTGRID_ENROLL_TOKEN="$tok")
  env PATH="$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE_PROXY" \
    AGENTGRID_DATA_DIR="$NODE_DATA" \
    AGENTGRID_NODE_NAME="e2e-node" \
    AGENTGRID_WORKSPACE_ROOT="$WORK" \
    AGENTGRID_REPOSITORY_ROOT="$REPOS" \
    AGENTGRID_ADAPTERS="mock" \
    AGENTGRID_MAX_CONCURRENCY="2" \
    RUST_LOG="info" \
    "${env_args[@]}" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
  NODE_PID=$!
}

wait_ready() {
  for _ in $(seq 1 40); do
    curl -fsS "$BASE_REAL/health/ready" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}

login() {
  jwt=$(curl -fsS -X POST "$BASE_REAL/v1/auth/login" \
    -H 'content-type: application/json' \
    -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  [ -n "$jwt" ] || { echo "login failed"; cat "$TMP/cp.log"; exit 1; }
}

mint_token() {
  ENROLL_TOKEN=$(curl -fsS -X POST "$BASE_REAL/v1/nodes/enrollment-token" \
    -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  [ -n "$ENROLL_TOKEN" ] || { echo "mint token failed"; exit 1; }
}

wait_node_online() {
  local st="none"
  for _ in $(seq 1 60); do
    st=$(curl -fsS "$BASE_REAL/v1/nodes" -H "authorization: Bearer $jwt" \
      | python3 -c 'import sys,json;ns=json.load(sys.stdin);print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st="none"
    [ "$st" = "online" ] && return 0
    sleep 0.5
  done
  echo "node never came online; status=$st"; cat "$TMP/node.log"; return 1
}

submit() {
  local prompt_json
  prompt_json=$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1")
  curl -fsS -X POST "$BASE_REAL/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$prompt_json,\"repository\":\"*\",\"adapter\":\"mock\",\"timeout_secs\":120}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}

wait_terminal() {
  STATUS=""
  for _ in $(seq 1 "$2"); do
    STATUS=$(curl -fsS "$BASE_REAL/v1/tasks/$1" -H "authorization: Bearer $jwt" \
      | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
    case "$STATUS" in
      succeeded|failed|cancelled|timed_out|lost) return 0;;
    esac
    sleep 1
  done
  return 1
}

echo ">> slow-network failure injection (delay=${AG_E2E_SLOW_MS:-250}ms/write)"

echo "  starting control plane on :7811"
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
# setup uses the real control-plane URL (not the slow proxy): /v1/auth/setup
# is a one-shot console op, run it without paying the throttle latency.
bootstrap_first_user "$TMP/cp.log" "$BASE_REAL" "$USER" "$PASS"
login
mint_token

echo "  starting throttle proxy :7820 -> :7811"
start_proxy
sleep 0.5

echo "  starting node pointed at proxy :7820"
start_node "$ENROLL_TOKEN"
wait_node_online || exit 1

echo "  submitting chatty task spam:200 (200 events through slow link)"
TID=$(submit "spam:200")
echo "  task $TID; polling terminal (timeout 120s)"
if wait_terminal "$TID" 120; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: expected succeeded, got $STATUS"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1; }

echo "  checking event continuity (expect 200 spam lines, contiguous sequences)"
# Grace: completion can overtake the last flush; poll events until stable.
EV=""
for _ in $(seq 1 30); do
  EV=$(curl -fsS "$BASE_REAL/v1/tasks/$TID/events" -H "authorization: Bearer $jwt" 2>/dev/null || true)
  n=$(printf '%s' "$EV" | python3 -c 'import sys,json
try:
  ev=json.load(sys.stdin)
  print(len([e for e in ev if e.get("type")=="stdout"]))
except Exception:
  print(0)' 2>/dev/null)
  [ "${n:-0}" -ge 200 ] && break
  sleep 1
done
n=$(printf '%s' "$EV" | python3 -c 'import sys,json
try:
  ev=json.load(sys.stdin)
  print(len([e for e in ev if e.get("type")=="stdout"]))
except Exception:
  print(0)' 2>/dev/null)
[ "${n:-0}" -ge 200 ] || { echo "  FAILED: only $n stdout events (expected 200)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1; }

printf '%s' "$EV" | python3 -c 'import sys,json
ev=json.load(sys.stdin)
seqs=sorted(e["sequence"] for e in ev if e.get("type")=="stdout")
assert len(seqs)>=200, f"too few: {len(seqs)}"
assert seqs==list(range(seqs[0],seqs[-1]+1)), f"non-contiguous: {seqs[:5]}..{seqs[-5:]}"
print(f"  {len(seqs)} stdout events, contiguous {seqs[0]}..{seqs[-1]}")'

echo "  B OK: chatty task completed under slow network, no gaps/dups"
echo ">> slow-net OK"
