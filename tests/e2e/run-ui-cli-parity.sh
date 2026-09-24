#!/usr/bin/env bash
# E2E (process-based CP+node, mock adapter): the plan 4.4 exit criterion —
# the same happy-path scenario (run → logs → succeeded → diff/patch,
# cancel → retry) driven through BOTH operator interfaces:
#
#   Part A — the `ag` CLI binary (run/show/logs/cancel/retry), and
#   Part B — the exact HTTP sequence the React web UI uses (web/src/api.ts):
#           POST /v1/tasks, GET task, GET events, GET events/stream (SSE),
#           GET artifact, POST cancel, POST retry.
#
# A headless browser is deliberately out of scope (flaky/heavy in CI); the
# UI is a thin client over these endpoints, so proving the UI-consumed
# contract end to end proves the UI can drive the scenario. Requires: curl
# and a Python 3 (`PYTHON3` overrides the binary name, default `python3`).
# No docker, no systemd — runs anywhere the debug binaries build.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7816}"
PORT="${AGENTGRID_PORT:-7816}"
USER="admin"
source "$ROOT/tests/e2e/lib-bootstrap.sh"
PASS="changeme"

for b in agentgrid-control-plane agentgrid-node-daemon adapter-mock ag; do
  [ -x "$BIN/$b" ] || { echo ">> skip: $BIN/$b not built (cargo build -p agentgrid-cli -p agentgrid-control-plane -p agentgrid-node-daemon -p agentgrid-adapters)"; exit 0; }
done

TMP="$(mktemp -d -t ag-e2e-parity-XXXXXX)"
CP_DB="$TMP/cp.db"
NODE_DATA="$TMP/node"
WORK="$TMP/work"
REPOS="$TMP/repos"
export HOME="$TMP/home"
mkdir -p "$NODE_DATA" "$WORK" "$REPOS" "$HOME"

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

AG="$BIN/ag --server $BASE"

start_cp() {
  AGENTGRID_LISTEN="127.0.0.1:$PORT" \
  AGENTGRID_DB="$CP_DB" \
  AGENTGRID_JWT_SECRET="e2e-stable-secret" \
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
    AGENTGRID_NODE_NAME="e2e-parity" \
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
    curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}

login() {
  jwt=$(curl -fsS -X POST "$BASE/v1/auth/login" \
    -H 'content-type: application/json' \
    -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
    | "${PYTHON3:-python3}" -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  [ -n "$jwt" ] || { echo "login failed"; cat "$TMP/cp.log"; exit 1; }
}

mint_token() {
  ENROLL_TOKEN=$(curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
    -H "authorization: Bearer $jwt" \
    | "${PYTHON3:-python3}" -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  [ -n "$ENROLL_TOKEN" ] || { echo "mint token failed"; exit 1; }
}

wait_node_online() {
  for _ in $(seq 1 60); do
    st=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" 2>/dev/null \
      | "${PYTHON3:-python3}" -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st="none"
    [ "$st" = "online" ] && return 0
    sleep 0.5
  done
  echo "node never came online; status=$st"; cat "$TMP/node.log"; return 1
}

task_status() {  # $1 = task id; prints status
  curl -fsS "$BASE/v1/tasks/$1" -H "authorization: Bearer $jwt" \
    | "${PYTHON3:-python3}" -c 'import sys,json;print(json.load(sys.stdin)["status"])'
}

wait_terminal() {  # $1 = seconds, $2 = task id
  STATUS=""
  for _ in $(seq 1 "$1"); do
    STATUS=$(task_status "$2")
    case "$STATUS" in
      succeeded|failed|cancelled|timed_out|lost) return 0;;
    esac
    sleep 1
  done
  return 1
}

echo ">> parity e2e: same scenario via ag CLI and via the web-UI HTTP contract"
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
login
mint_token
start_node "$ENROLL_TOKEN"
wait_node_online || exit 1

# ---------------- Part A: the `ag` CLI ----------------
echo ">> [CLI] login"
$AG login "$USER" "$PASS" >/dev/null || { echo "  FAILED: ag login"; exit 1; }

echo ">> [CLI] run -> show -> logs -> succeeded"
TID_CLI=$($AG run '*' 'spam:5' | tail -n 1)
[ -n "$TID_CLI" ] || { echo "  FAILED: ag run printed no task id"; exit 1; }
echo "  task $TID_CLI"
$AG show "$TID_CLI" >/dev/null || { echo "  FAILED: ag show"; exit 1; }
wait_terminal 90 "$TID_CLI" || { echo "  FAILED: task never terminal (status=$STATUS)"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: expected succeeded, got $STATUS"; exit 1; }
$AG logs --no-color "$TID_CLI" | grep -q "spam line" \
  || { echo "  FAILED: ag logs missing spam lines"; exit 1; }
echo "  CLI happy path OK (run/show/logs/succeeded)"

echo ">> [CLI] cancel -> retry on a sleep task"
TID_C2=$($AG run '*' 'sleep:30' | tail -n 1)
sleep 3  # let the node pick it up
$AG cancel "$TID_C2" >/dev/null || { echo "  FAILED: ag cancel"; exit 1; }
wait_terminal 60 "$TID_C2" || { echo "  FAILED: cancelled task never terminal"; exit 1; }
[ "$STATUS" = "cancelled" ] || { echo "  FAILED: expected cancelled, got $STATUS"; exit 1; }
$AG retry "$TID_C2" >/dev/null || { echo "  FAILED: ag retry"; exit 1; }
sleep 3
ST2=$(task_status "$TID_C2")
case "$ST2" in
  queued|running) echo "  retry re-queued the task ($ST2)";;
  *) echo "  FAILED: expected queued/running after retry, got $ST2"; exit 1;;
esac
$AG cancel "$TID_C2" >/dev/null || true
wait_terminal 60 "$TID_C2" || true
echo "  CLI cancel/retry OK"

# ---------------- Part B: the web-UI HTTP contract ----------------
echo ">> [UI] POST /v1/tasks (NewTask form)"
TID_UI=$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' 'spam:5' | {
  read -r prompt_json
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$prompt_json,\"repository\":\"*\",\"adapter\":\"mock\"}" \
    | "${PYTHON3:-python3}" -c 'import sys,json;print(json.load(sys.stdin)["id"])'
})
echo "  task $TID_UI"

echo ">> [UI] GET /v1/tasks/{id} (TaskDetails header)"
curl -fsS "$BASE/v1/tasks/$TID_UI" -H "authorization: Bearer $jwt" \
  | "${PYTHON3:-python3}" -c 'import sys,json;d=json.load(sys.stdin);assert d["id"];print("  details OK, status:",d["status"])'

echo ">> [UI] GET /v1/tasks/{id}/events (event list)"
curl -fsS "$BASE/v1/tasks/$TID_UI/events?limit=100" -H "authorization: Bearer $jwt" \
  | "${PYTHON3:-python3}" -c 'import sys,json;d=json.load(sys.stdin);ev=d.get("items",d) if isinstance(d,dict) else d;print(f"  {len(ev)} events listed")'

echo ">> [UI] GET /v1/tasks/{id}/events/stream (live SSE)"
curl -fsS -N --max-time 20 "$BASE/v1/tasks/$TID_UI/events/stream?after_ingest=0" \
  -H "authorization: Bearer $jwt" -o "$TMP/sse.txt" || true
grep -q "^data:" "$TMP/sse.txt" \
  || { echo "  FAILED: SSE stream carried no data: lines"; cat "$TMP/sse.txt"; exit 1; }
echo "  SSE stream OK ($(grep -c '^data:' "$TMP/sse.txt") data lines)"

wait_terminal 90 "$TID_UI" || { echo "  FAILED: UI task never terminal"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: expected succeeded, got $STATUS"; exit 1; }

echo ">> [UI] GET /v1/tasks/{id}/artifacts/agent-raw-output.log (diff/log view)"
curl -fsS "$BASE/v1/tasks/$TID_UI/artifacts/agent-raw-output.log" \
  -H "authorization: Bearer $jwt" | grep -q "spam line" \
  || { echo "  FAILED: artifact download missing log content"; exit 1; }
echo "  artifact download OK"

echo ">> [UI] POST cancel -> POST retry on a sleep task"
TID_U2=$(curl -fsS -X POST "$BASE/v1/tasks" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d '{"prompt":"sleep:30","repository":"*","adapter":"mock"}' \
  | "${PYTHON3:-python3}" -c 'import sys,json;print(json.load(sys.stdin)["id"])')
sleep 3
curl -fsS -X POST "$BASE/v1/tasks/$TID_U2/cancel" -H "authorization: Bearer $jwt" >/dev/null \
  || { echo "  FAILED: UI cancel"; exit 1; }
wait_terminal 60 "$TID_U2" || { echo "  FAILED: UI-cancelled task never terminal"; exit 1; }
[ "$STATUS" = "cancelled" ] || { echo "  FAILED: expected cancelled, got $STATUS"; exit 1; }
curl -fsS -X POST "$BASE/v1/tasks/$TID_U2/retry" -H "authorization: Bearer $jwt" >/dev/null \
  || { echo "  FAILED: UI retry"; exit 1; }
sleep 3
STU2=$(task_status "$TID_U2")
case "$STU2" in
  queued|running) echo "  UI retry re-queued the task ($STU2)";;
  *) echo "  FAILED: expected queued/running after UI retry, got $STU2"; exit 1;;
esac
curl -fsS -X POST "$BASE/v1/tasks/$TID_U2/cancel" -H "authorization: Bearer $jwt" >/dev/null || true
wait_terminal 60 "$TID_U2" || true
echo "  UI cancel/retry OK"

echo ">> parity e2e OK: CLI and web-UI contract drive the same scenario"
