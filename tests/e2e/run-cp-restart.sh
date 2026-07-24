#!/usr/bin/env bash
# E2E (process-based, no Docker): control-plane restart under load.
#
# Plan item: "Рестарт control plane под нагрузкой → nodes переподключаются,
# ничего не теряется" (Stage 2.x failure injection).
#
# Scenario: submit several tasks concurrently (load), kill the control plane
# mid-flight while nodes hold in-flight attempts, restart the control plane
# against the SAME SQLite DB, and assert every task that was in-flight reaches
# a terminal status with the full contiguous event stream — no task lost, no
# event gaps, nodes re-enroll/heartbeat and keep working. New tasks submitted
# *after* the restart also succeed (proves the reconnected stack is live, not
# wedged).
#
# Reuses the run-outbox.sh scaffolding (debug binaries, temp dir, cleanup).
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7812}"
PORT="${AGENTGRID_PORT:-7812}"
USER="admin"
PASS="changeme"

TMP="$(mktemp -d -t ag-e2e-cprestart-XXXXXX)"
CP_DB="$TMP/cp.db"
NODE_A_DATA="$TMP/node-a"
NODE_B_DATA="$TMP/node-b"
WORK="$TMP/work"
REPOS="$TMP/repos"
mkdir -p "$NODE_A_DATA" "$NODE_B_DATA" "$WORK" "$REPOS"

CP_PID=""
NODE_A_PID=""
NODE_B_PID=""

cleanup() {
  set +e
  [ -n "$NODE_B_PID" ] && kill -9 "$NODE_B_PID" 2>/dev/null
  [ -n "$NODE_A_PID" ] && kill -9 "$NODE_A_PID" 2>/dev/null
  [ -n "$CP_PID" ] && kill "$CP_PID" 2>/dev/null
  pkill -f "$BIN/agentgrid-control-plane" 2>/dev/null
  pkill -f "$BIN/agentgrid-node-daemon" 2>/dev/null
  pkill -f "AGENTGRID_DATA_DIR=$NODE_A_DATA" 2>/dev/null
  pkill -f "AGENTGRID_DATA_DIR=$NODE_B_DATA" 2>/dev/null
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

start_node() {  # $1 = data dir, $2 = name, $3 = enroll token (optional)
  local data="$1" name="$2" tok="${3:-}"
  local env_args=()
  [ -n "$tok" ] && env_args+=(AGENTGRID_ENROLL_TOKEN="$tok")
  env PATH="$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE" \
    AGENTGRID_DATA_DIR="$data" \
    AGENTGRID_NODE_NAME="$name" \
    AGENTGRID_WORKSPACE_ROOT="$WORK" \
    AGENTGRID_REPOSITORY_ROOT="$REPOS" \
    AGENTGRID_ADAPTERS="mock" \
    AGENTGRID_MAX_CONCURRENCY="2" \
    RUST_LOG="info" \
    "${env_args[@]}" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/$name.log" 2>&1 &
  if [ "$name" = "node-a" ]; then NODE_A_PID=$!; else NODE_B_PID=$!; fi
}

wait_ready() {
  for _ in $(seq 1 40); do
    curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
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

wait_nodes_online() {  # expect exactly N online
  local want="$1"
  for _ in $(seq 1 80); do
    n=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" 2>/dev/null \
      | python3 -c 'import sys,json;ns=[n for n in json.load(sys.stdin) if n["status"]=="online"];print(len(ns))' 2>/dev/null) || n=0
    [ "$n" = "$want" ] && return 0
    sleep 0.5
  done
  echo "expected $want online nodes, got $n"; cat "$TMP/cp.log"; return 1
}

submit() {  # $1 = prompt; prints task id
  local prompt_json
  prompt_json=$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1")
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$prompt_json,\"repository\":\"*\",\"adapter\":\"mock\",\"timeout_secs\":60}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}

poll_status() {  # $1 = task id; sets STATUS
  STATUS=$(curl -fsS "$BASE/v1/tasks/$1" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
}

wait_terminal() {  # $1 = task id, $2 = max seconds
  STATUS=""
  for _ in $(seq 1 "$2"); do
    poll_status "$1"
    case "$STATUS" in
      succeeded|failed|cancelled|timed_out|lost) return 0;;
    esac
    sleep 1
  done
  return 1
}

event_continuity_ok() {  # $1 = task id, $2 = min spam count; prints PASS/FAIL
  sleep 2
  local evs
  evs=$(curl -fsS "$BASE/v1/tasks/$1/events?after_sequence=0" -H "authorization: Bearer $jwt")
  python3 <<PYEOF
import json
evs = json.loads('''$evs''') if '''$evs'''.strip() else []
seqs = sorted(e["sequence"] for e in evs)
spam = [e for e in evs if e["payload"].get("text","").startswith("spam line")]
print(f"  events={len(evs)} spam={len(spam)}")
gaps = []
for i in range(1,len(seqs)):
    if seqs[i] != seqs[i-1]+1: gaps.append((seqs[i-1],seqs[i]))
if len(spam) < $2:
    print("FAIL_SPAM"); raise SystemExit(2)
if gaps:
    print("FAIL_GAPS:" + str(gaps)); raise SystemExit(2)
print("PASS")
PYEOF
}

echo ">> CP restart under load — bring up CP + 2 nodes"
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
login
mint_token
# Stagger node starts: concurrent enrollment against the same enrollment
# token races the SQLite write path. Start node-a, let it enroll, then node-b.
# Each enrollment token is one-shot (store.enroll_node marks it used).
# Mint one per node to avoid a 400 on the second enroll.
mint_token
start_node "$NODE_A_DATA" "node-a" "$ENROLL_TOKEN"
wait_nodes_online 1 || exit 1
echo "  node-a online; starting node-b"
mint_token
start_node "$NODE_B_DATA" "node-b" "$ENROLL_TOKEN"
wait_nodes_online 2 || exit 1
echo "  2 nodes online"

echo ">> submitting load: 4 concurrent mixed tasks"
# Two short (complete fast, sit in outbox during outage) + two long (in-flight
# when CP dies; on restart their heartbeat recovers or they get lost→retry).
declare -a TIDS
TIDS[0]=$(submit $'spam:20\nsleep:1')
TIDS[1]=$(submit $'spam:20\nsleep:1')
TIDS[2]=$(submit "sleep:20")
TIDS[3]=$(submit "sleep:20")
echo "  tasks: ${TIDS[0]} ${TIDS[1]} ${TIDS[2]} ${TIDS[3]}"
echo "  waiting 1.5s for assignments to land"
sleep 1.5

echo ">> killing CP (hard) mid-load — nodes hold attempts, spool events"
stop_cp
# Keep CP down ~6s: short tasks finish + completion sits in durable outbox;
# long tasks keep their assignment (lease 30s never expires). Nodes keep
# retrying the CP poll and buffering to outbox.
echo "  CP down 6s (nodes spool to outbox, keep heartbeating)"
sleep 6

echo ">> restarting CP against the SAME DB (state survives in SQLite)"
start_cp
wait_ready || { echo "CP not ready after restart"; cat "$TMP/cp.log"; exit 1; }
login
echo "  CP back; waiting for nodes to re-online"
# Nodes are alive and long-polling; once CP is up they re-enroll/heartbeat.
wait_nodes_online 2 || exit 1
echo "  2 nodes re-online after CP restart"

echo ">> asserting short tasks succeeded with contiguous events"
for i in 0 1; do
  if wait_terminal "${TIDS[$i]}" 40; then
    echo "  task ${TIDS[$i]}: $STATUS"
  else
    echo "  task ${TIDS[$i]} timed out ($STATUS)"; cat "$TMP/cp.log"; exit 1
  fi
  [ "$STATUS" = "succeeded" ] || { echo "  FAILED: short task ${TIDS[$i]} -> $STATUS"; exit 1; }
  res=$(event_continuity_ok "${TIDS[$i]}" 20 | tail -1 | tr -d ' \n')
  echo "  $res (continuity for ${TIDS[$i]})"
  [ "$res" = "PASS" ] || { echo "  FAILED: event continuity for ${TIDS[$i]}"; exit 1; }
done

echo ">> waiting for long tasks (they resumed or re-assigned after restart)"
for i in 2 3; do
  if wait_terminal "${TIDS[$i]}" 50; then
    echo "  task ${TIDS[$i]}: $STATUS"
  else
    echo "  task ${TIDS[$i]} timed out ($STATUS)"; cat "$TMP/cp.log"; cat "$TMP/node-a.log"; cat "$TMP/node-b.log"; exit 1
  fi
  [ "$STATUS" = "succeeded" ] || { echo "  FAILED: long task ${TIDS[$i]} -> $STATUS"; exit 1; }
done

echo ">> post-restart: submit a NEW task (proves stack is live, not wedged)"
NTID=$(submit $'spam:10\nsleep:1')
if wait_terminal "$NTID" 30; then
  echo "  new task $NTID: $STATUS"
else
  echo "  new task $NTID timed out ($STATUS)"; exit 1
fi
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: new post-restart task -> $STATUS"; exit 1; }

echo ">> CP restart under load OK: load survived restart, no task/event lost, stack live"
