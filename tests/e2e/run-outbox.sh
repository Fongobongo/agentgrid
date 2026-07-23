#!/usr/bin/env bash
# Stage 2.1 E2E (process-based, no Docker): durable delivery under daemon kill
# and control-plane outage.
#
# Scenario A — kill -9 node + completion durability:
#   task `sleep:4` is assigned; we stop the CP so the node's completion send
#   fails (completion sits in completions.jsonl); kill -9 the node; restart CP;
#   restart the node → startup redelivers the completion → task succeeds.
#
# Scenario B — network disconnect (node alive) + event continuity:
#   task `spam:100\nsleep:3\nspam:100`; stop CP mid-flight (node spools second
#   batch + completion to outbox); restart CP (node alive) → flush loop
#   redelivers from outbox; assert task succeeds and all 200 events present with
#   contiguous sequences (no dup/loss).
#
# Uses the already-built debug binaries (no disk-heavy Docker build). Cleanup on
# exit kills both processes and removes the temp dir.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7811}"
PORT="${AGENTGRID_PORT:-7811}"
USER="admin"
PASS="changeme"

TMP="$(mktemp -d -t ag-e2e-outbox-XXXXXX)"
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
  # Keep the temp dir on failure for post-mortem if AG_E2E_KEEP=1.
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
    curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}

# Stop the control plane fast: SIGTERM, then SIGKILL after a short grace so a
# long-poll / graceful-shutdown does not stretch the outage window past the
# ack/lease (the test simulates a hard network failure, not a polite drain).
stop_cp() {
  [ -n "$CP_PID" ] || return 0
  kill "$CP_PID" 2>/dev/null || true
  for _ in $(seq 1 10); do
    kill -0 "$CP_PID" 2>/dev/null || { CP_PID=""; return 0; }
    sleep 0.2
  done
  kill -9 "$CP_PID" 2>/dev/null || true
  # Reap the zombie so `wait` later does not block.
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

wait_node_online() {
  local st="none"
  for _ in $(seq 1 60); do
    st=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
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

wait_terminal() {  # $1 = task id, $2 = max seconds; sets STATUS
  STATUS=""
  for _ in $(seq 1 "$2"); do
    STATUS=$(curl -fsS "$BASE/v1/tasks/$1" -H "authorization: Bearer $jwt" \
      | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
    case "$STATUS" in
      succeeded|failed|cancelled|timed_out|lost) return 0;;
    esac
    sleep 1
  done
  return 1
}

poll_status() {  # $1 = task id; sets STATUS (does not loop)
  STATUS=$(curl -fsS "$BASE/v1/tasks/$1" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
}

echo ">> Scenario A: kill -9 node + completion durability"

echo "  starting control plane"
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
login
echo "  minting enrollment token"
mint_token
echo "  starting node (enroll)"
start_node "$ENROLL_TOKEN"
wait_node_online || exit 1

echo "  submitting task sleep:4 (short: record completion fast, keep CP-down <30s)"
TID=$(submit "sleep:4")
echo "  task $TID assigned; waiting 2s for it to start"
sleep 2

echo "  stopping CP (so completion send fails → outbox)"
stop_cp

# Wait deterministically until the adapter finishes and the node records the
# completion to the durable outbox (the send to the now-down CP fails, so the
# record stays). Poll completions.jsonl rather than sleeping a fixed window.
# We kill the node immediately after, keeping the CP-down window well under
# the 30s heartbeat-staleness threshold so CP won't mark the attempt lost on
# restart.
echo "  waiting for completion to land in outbox (adapter sleep:4)"
for _ in $(seq 1 30); do
  if [ -s "$NODE_DATA/outbox/completions.jsonl" ]; then break; fi
  sleep 1
done
[ -s "$NODE_DATA/outbox/completions.jsonl" ] || { echo "  completion never recorded; node log:"; cat "$TMP/node.log"; exit 1; }
echo "  completion in outbox: $(cat "$NODE_DATA/outbox/completions.jsonl")"

echo "  kill -9 node (completion is durable in completions.jsonl)"
kill -9 "$NODE_PID" 2>/dev/null; wait "$NODE_PID" 2>/dev/null || true; NODE_PID=""

echo "  restarting CP (same DB)"
start_cp
wait_ready || { echo "CP not ready after restart"; cat "$TMP/cp.log"; exit 1; }
# Same JWT secret → existing token still valid; re-login to be safe.
login

echo "  restarting node (saved credential; no token) → redelivers completion"
start_node ""
wait_node_online || exit 1

echo "  polling task for terminal status (timeout 40s)"
if wait_terminal "$TID" 60; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "succeeded" ] || { echo "  A FAILED: expected succeeded, got $STATUS"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1; }
echo "  A OK: completion survived kill -9 + CP outage"

echo ">> Scenario B: network disconnect (node alive) + event continuity"

echo "  submitting task spam+sleep+spam (200 events)"
TID=$(submit $'spam:100\nsleep:3\nspam:100')
echo "  task $TID; waiting 1s for first 100 events to flush"
sleep 1

echo "  stopping CP for 4s (node spools second batch + completion)"
stop_cp
sleep 4

echo "  restarting CP (node alive → flush loop redelivers from outbox)"
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
login

echo "  polling task for terminal status (timeout 40s)"
if wait_terminal "$TID" 60; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "succeeded" ] || { echo "  B FAILED: expected succeeded, got $STATUS"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1; }

echo "  checking event continuity (expect 200 spam lines, contiguous sequences)"
# Small grace: the node may finish draining the event tail just after the CP
# marks the task `succeeded` (the completion can overtake the last flush).
sleep 2
EVS=$(curl -fsS "$BASE/v1/tasks/$TID/events?after_sequence=0" -H "authorization: Bearer $jwt")
python3 <<PYEOF
import json
evs = json.loads('''$EVS''') if '''$EVS'''.strip() else []
seqs = sorted(e["sequence"] for e in evs)
spam = [e for e in evs if e["payload"].get("text","").startswith("spam line")]
print(f"  events={len(evs)} spam={len(spam)}")
gaps = []
for i in range(1,len(seqs)):
    if seqs[i] != seqs[i-1]+1: gaps.append((seqs[i-1],seqs[i]))
if len(spam) != 200:
    print("  B FAILED: expected 200 spam events, got", len(spam)); raise SystemExit(1)
if gaps:
    print("  B FAILED: sequence gaps:", gaps); raise SystemExit(1)
print("  B OK: 200 events, no gaps/dup")
PYEOF

echo ">> Scenario D: variable CP outage (network failure injection), no dup/gap"
# Failure injection: keep the CP down for AG_E2E_OUTAGE_SECS (default 10s)
# while the node keeps streaming; on CP return the outbox must redeliver
# contiguous events with no gap and no dup. The task is short (a few seconds)
# so it finishes *during* the outage and its completion sits in the durable
# completions spool; the outage window is sized so the ack/lease (30s) never
# expires before the CP comes back (total elapsed < lease).
OUTAGE="${AG_E2E_OUTAGE_SECS:-10}"
echo "  submitting task spam+spam+sleep (200 events, short)"
TID=$(submit $'spam:100\nspam:100\nsleep:3')
echo "  task $TID; waiting 1s for the node to start it"
sleep 1

echo "  stopping CP for ${OUTAGE}s (node alive, spools)"
stop_cp
sleep "$OUTAGE"

echo "  restarting CP (node alive → flush loop redelivers)"
T_D0=$(date +%s)
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
T_D1=$(date +%s)
echo "  CP restart took $((T_D1 - T_D0))s"
login

echo "  polling task for terminal status (timeout 60s)"
if wait_terminal "$TID" 80; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "succeeded" ] || { echo "  D FAILED: expected succeeded, got $STATUS"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1; }

echo "  checking event continuity (expect 200 spam, contiguous sequences)"
sleep 2
EVS=$(curl -fsS "$BASE/v1/tasks/$TID/events?after_sequence=0" -H "authorization: Bearer $jwt")
python3 <<PYEOF
import json
evs = json.loads('''$EVS''') if '''$EVS'''.strip() else []
seqs = sorted(e["sequence"] for e in evs)
spam = [e for e in evs if e["payload"].get("text","").startswith("spam line")]
print(f"  events={len(evs)} spam={len(spam)}")
gaps = []
for i in range(1,len(seqs)):
    if seqs[i] != seqs[i-1]+1: gaps.append((seqs[i-1],seqs[i]))
if len(spam) != 200:
    print("  D FAILED: expected 200 spam events, got", len(spam)); raise SystemExit(1)
if gaps:
    print("  D FAILED: sequence gaps:", gaps); raise SystemExit(1)
print("  D OK: 200 events, no gaps/dup after ${OUTAGE}s outage")
PYEOF

echo ">> Scenario C: kill node mid-running → lost → retry → succeeded"

# Fresh CP + node for this scenario (clean state).
echo "  finding CP/node state"
[ -n "$CP_PID" ] || { echo "  C FAILED: CP not running"; exit 1; }
# Restart the node so a fresh long task is assigned cleanly.
[ -n "$NODE_PID" ] && { kill -9 "$NODE_PID" 2>/dev/null; wait "$NODE_PID" 2>/dev/null || true; }
unset ENROLL_TOKEN
start_node ""
wait_node_online || exit 1

echo "  submitting task sleep:8 (long, will be killed)"
TID=$(submit "sleep:8")
echo "  task $TID; polling for running (timeout 15s)"
reach=0
for _ in $(seq 1 15); do
  poll_status "$TID"
  if [ "$STATUS" = "running" ]; then reach=1; break; fi
  sleep 1
done
[ "$reach" = "1" ] || { echo "  C FAILED: never reached running before kill, got $STATUS"; exit 1; }

echo "  kill -9 node (attempt should go lost → task failed/node_lost)"
kill -9 "$NODE_PID" 2>/dev/null; wait "$NODE_PID" 2>/dev/null || true; NODE_PID=""

echo "  polling for failed/lost (maintenance marks node offline after ~30s; timeout 50s)"
# Override wait_terminal's terminal set: we want failed-or-lost here.
for _ in $(seq 1 50); do
  poll_status "$TID"
  case "$STATUS" in failed|lost) break;; esac
  sleep 1
done
if [ "$STATUS" = "failed" ]; then
  echo "  status after kill: failed (expected)"
else
  echo "  C FAILED: expected failed after node-kill, got $STATUS"; cat "$TMP/cp.log"; exit 1
fi

echo "  retrying task $TID"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/tasks/$TID/retry" \
  -H "authorization: Bearer $jwt")
[ "$code" = "200" ] || { echo "  C FAILED: retry returned $code"; exit 1; }

echo "  restarting node → fresh attempt should run to succeeded"
unset ENROLL_TOKEN
start_node ""
wait_node_online || exit 1

echo "  polling task for terminal status (timeout 40s)"
if wait_terminal "$TID" 60; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "succeeded" ] || { echo "  C FAILED: expected succeeded after retry, got $STATUS"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1; }
echo "  C OK: lost attempt → retry → succeeded"

echo ">> E2E OK (all scenarios)"
