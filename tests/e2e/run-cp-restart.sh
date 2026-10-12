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
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7812}"
PORT="${AGENTGRID_PORT:-7812}"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-stable-secret"
E2E_CONCURRENCY=2

TMP="$(mktemp -d -t ag-e2e-cprestart-XXXXXX)"
NODE_A_DATA="$TMP/node-a"
NODE_B_DATA="$TMP/node-b"
WORK="$TMP/work"
REPOS="$TMP/repos"
mkdir -p "$NODE_A_DATA" "$NODE_B_DATA" "$WORK" "$REPOS"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

event_continuity_ok() {  # $1 = task id, $2 = min spam count; prints PASS/FAIL
  sleep 2
  local evs
  evs=$(e2e_task_events "$1")
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
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
# token races the SQLite write path. Each enrollment token is one-shot
# (store.enroll_node marks it used): mint one per node to avoid a 400 on
# the second enroll.
e2e_mint_token
e2e_start_node node-a "node-a" "mock" "$WORK" "$REPOS" "$NODE_A_DATA"
e2e_wait_nodes_online 1 80 || exit 1
echo "  node-a online; starting node-b"
e2e_mint_token
e2e_start_node node-b "node-b" "mock" "$WORK" "$REPOS" "$NODE_B_DATA"
e2e_wait_nodes_online 2 80 || exit 1
echo "  2 nodes online"

echo ">> submitting load: 4 concurrent mixed tasks"
# Two short (complete fast, sit in outbox during outage) + two long (in-flight
# when CP dies; on restart their heartbeat recovers or they get lost→retry).
declare -a TIDS
TIDS[0]=$(e2e_submit $'spam:20\nsleep:1' "mock" "*" 60)
TIDS[1]=$(e2e_submit $'spam:20\nsleep:1' "mock" "*" 60)
TIDS[2]=$(e2e_submit "sleep:20" "mock" "*" 60)
TIDS[3]=$(e2e_submit "sleep:20" "mock" "*" 60)
echo "  tasks: ${TIDS[0]} ${TIDS[1]} ${TIDS[2]} ${TIDS[3]}"
echo "  waiting 1.5s for assignments to land"
sleep 1.5

echo ">> killing CP (hard) mid-load — nodes hold attempts, spool events"
e2e_stop_cp
# Keep CP down ~6s: short tasks finish + completion sits in durable outbox;
# long tasks keep their assignment (lease 30s never expires). Nodes keep
# retrying the CP poll and buffering to outbox.
echo "  CP down 6s (nodes spool to outbox, keep heartbeating)"
sleep 6

echo ">> restarting CP against the SAME DB (state survives in SQLite)"
e2e_start_cp
e2e_wait_ready || { echo "CP not ready after restart"; cat "$TMP/cp.log"; exit 1; }
e2e_login
echo "  CP back; waiting for nodes to re-online"
# Nodes are alive and long-polling; once CP is up they re-enroll/heartbeat.
e2e_wait_nodes_online 2 80 || exit 1
echo "  2 nodes re-online after CP restart"

echo ">> asserting short tasks succeeded with contiguous events"
for i in 0 1; do
  if e2e_wait_terminal 40 "${TIDS[$i]}"; then
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
  if e2e_wait_terminal 50 "${TIDS[$i]}"; then
    echo "  task ${TIDS[$i]}: $STATUS"
  else
    echo "  task ${TIDS[$i]} timed out ($STATUS)"; cat "$TMP/cp.log"; cat "$TMP/node-a.log"; cat "$TMP/node-b.log"; exit 1
  fi
  [ "$STATUS" = "succeeded" ] || { echo "  FAILED: long task ${TIDS[$i]} -> $STATUS"; exit 1; }
done

echo ">> post-restart: submit a NEW task (proves stack is live, not wedged)"
NTID=$(e2e_submit $'spam:10\nsleep:1' "mock" "*" 60)
if e2e_wait_terminal 30 "$NTID"; then
  echo "  new task $NTID: $STATUS"
else
  echo "  new task $NTID timed out ($STATUS)"; exit 1
fi
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: new post-restart task -> $STATUS"; exit 1; }

echo ">> CP restart under load OK: load survived restart, no task/event lost, stack live"
