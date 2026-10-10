#!/usr/bin/env bash
# E2E (process-based, no Docker): one Agentgrid task runs on the Zeroshot
# cluster path end-to-end (Stage 10 / Exit 10 shape).
#
# The executor is `tests/e2e/zeroshot-stub.sh` (contract-conformant; see its
# header for the honest simplifications vs a real binary). A real-binary run
# is one env override away:
#   AGENTGRID_ZEROSHOT_BIN=/usr/local/bin/zeroshot bash tests/e2e/run-zeroshot.sh
# (needs docker + a real binary; CI uses the stub).
#
# Asserts: happy path (succeeded + cluster_created event + result event +
# zeroshot-result.json artifact + commit), failure path (`fail:3` →
# infrastructure is NOT blamed: agent exit 3, kill marker present), cancel
# path (sleep task → POST cancel → cancelled + kill marker present).
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

command -v docker >/dev/null 2>&1 || { echo "skip: docker not on PATH (zeroshot probe needs the runtime)"; exit 77; }

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7815}"
PORT="${AGENTGRID_PORT:-7815}"
USER="admin"
PASS="changeme"
source "$ROOT/tests/e2e/lib-bootstrap.sh"

TMP="$(mktemp -d -t ag-e2e-zeroshot-XXXXXX)"
CP_DB="$TMP/cp.db"
NODE_DATA="$TMP/node"
WORK="$TMP/work"
REPOS="$TMP/repos"
mkdir -p "$NODE_DATA" "$WORK" "$REPOS"

STUB_STATE="$TMP/stub-state"
mkdir -p "$STUB_STATE" "$TMP/stubbin"
cp "$ROOT/tests/e2e/zeroshot-stub.sh" "$TMP/stubbin/zeroshot"
chmod +x "$TMP/stubbin/zeroshot"
export ZEROSHOT_STUB_STATE="$STUB_STATE"
# A real binary overrides the stub without touching the script.
ZEROSHOT_BIN="${AGENTGRID_ZEROSHOT_BIN:-$TMP/stubbin/zeroshot}"

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
  AGENTGRID_JWT_SECRET="e2e-zeroshot-secret" \
  AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
  nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
  CP_PID=$!
}

start_node() {
  env PATH="$TMP/stubbin:$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE" \
    AGENTGRID_DATA_DIR="$NODE_DATA" \
    AGENTGRID_NODE_NAME="e2e-zeroshot" \
    AGENTGRID_WORKSPACE_ROOT="$WORK" \
    AGENTGRID_REPOSITORY_ROOT="$REPOS" \
    AGENTGRID_ADAPTERS="zeroshot" \
    AGENTGRID_ZEROSHOT_BIN="$ZEROSHOT_BIN" \
    AGENTGRID_MAX_CONCURRENCY="2" \
    RUST_LOG="info" \
    AGENTGRID_ENROLL_TOKEN="$ENROLL_TOKEN" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
  NODE_PID=$!
}

wait_node_online() {
  for _ in $(seq 1 60); do
    st=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" 2>/dev/null \
      | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st="none"
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
    -d "{\"prompt\":$prompt_json,\"repository\":\"*\",\"adapter\":\"zeroshot\",\"timeout_secs\":120}" \
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

wait_status() {  # $1 = want, $2 = task, $3 = timeout
  for _ in $(seq 1 "$3"); do
    poll_status "$2"
    [ "$STATUS" = "$1" ] && return 0
    sleep 1
  done
  return 1
}

events_contain() {  # $1 = task id, $2 = python predicate over evs list -> prints yes/no
  curl -fsS "$BASE/v1/tasks/$1/events?after_sequence=0" -H "authorization: Bearer $jwt" \
    | python3 -c "import sys,json;evs=json.load(sys.stdin);print('yes' if ($2) else 'no')"
}

echo ">> zeroshot E2E via $ZEROSHOT_BIN"
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
login
mint_token
start_node
wait_node_online || exit 1

echo ">> happy path: cluster run streams events, exports results"
T1=$(submit "zeroshot e2e hello")
echo "  task $T1"
wait_terminal 120 "$T1" || { echo "  task never terminal: $STATUS"; cat "$TMP/node.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: expected succeeded, got $STATUS"; cat "$TMP/node.log"; exit 1; }
[ "$(events_contain "$T1" "any(e.get('payload',{}).get('kind')=='cluster_created' for e in evs)")" = "yes" ] \
  || { echo "  FAILED: no cluster_created event"; exit 1; }
echo "  cluster_created event present"
[ -n "$(ls "$STUB_STATE"/started-* 2>/dev/null)" ] \
  || { echo "  FAILED: stub run marker missing"; ls "$STUB_STATE"; exit 1; }
[ -n "$(ls "$STUB_STATE"/exported-* 2>/dev/null)" ] \
  || { echo "  FAILED: stub export marker missing"; ls "$STUB_STATE"; exit 1; }
echo "  run + export markers present"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/tasks/$T1/artifacts/zeroshot-result.json" -H "authorization: Bearer $jwt")
[ "$code" = "200" ] || { echo "  FAILED: zeroshot-result.json artifact HTTP $code"; exit 1; }
echo "  zeroshot-result.json artifact downloadable"

echo ">> failure path: fail:3 is the agent's failure, kill still runs"
T2=$(submit "fail:3")
wait_terminal 120 "$T2" || { echo "  task never terminal: $STATUS"; exit 1; }
[ "$STATUS" = "failed" ] || { echo "  FAILED: expected failed, got $STATUS"; exit 1; }
[ -n "$(ls "$STUB_STATE"/killed-* 2>/dev/null)" ] \
  || { echo "  FAILED: no kill marker after failed run (cancel-total)"; ls "$STUB_STATE"; exit 1; }
echo "  failed + kill marker present"

echo ">> cancel path: sleep task cancelled, cluster killed"
rm -f "$STUB_STATE"/killed-*
T3=$(submit "sleep:60")
wait_status running "$T3" 60 || { echo "  task never running: $STATUS"; exit 1; }
curl -fsS -X POST "$BASE/v1/tasks/$T3/cancel" -H "authorization: Bearer $jwt" >/dev/null \
  || { echo "  cancel POST failed"; exit 1; }
wait_terminal 60 "$T3" || { echo "  cancelled task never terminal: $STATUS"; exit 1; }
[ "$STATUS" = "cancelled" ] || { echo "  FAILED: expected cancelled, got $STATUS"; exit 1; }
[ -n "$(ls "$STUB_STATE"/killed-* 2>/dev/null)" ] \
  || { echo "  FAILED: no kill marker after cancel (cancel-total)"; ls "$STUB_STATE"; exit 1; }
echo "  cancelled + kill marker present"

echo ">> zeroshot E2E OK (happy + failure + cancel-total)"
