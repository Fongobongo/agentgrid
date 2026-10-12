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
E2E_JWT_SECRET="e2e-stable-secret"
E2E_CONCURRENCY=2

TMP="$(mktemp -d -t ag-e2e-outbox-XXXXXX)"
NODE_DATA="$TMP/node"
WORK="$TMP/work"
REPOS="$TMP/repos"
mkdir -p "$NODE_DATA" "$WORK" "$REPOS"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

echo ">> Scenario A: kill -9 node + completion durability"

echo "  starting control plane"
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
echo "  minting enrollment token"
e2e_mint_token
echo "  starting node (enroll)"
e2e_start_node node "e2e-node" "mock" "$WORK" "$REPOS" "$NODE_DATA"
e2e_wait_nodes_online 1 || exit 1

echo "  submitting task sleep:4 (short: record completion fast, keep CP-down <30s)"
TID=$(e2e_submit "sleep:4" "mock" "*" 60)
echo "  task $TID assigned; waiting 2s for it to start"
sleep 2

echo "  stopping CP (so completion send fails → outbox)"
e2e_stop_cp

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
e2e_kill_node

echo "  restarting CP (same DB)"
e2e_start_cp
e2e_wait_ready || { echo "CP not ready after restart"; cat "$TMP/cp.log"; exit 1; }
# Same JWT secret → existing token still valid; re-login to be safe.
e2e_login

echo "  restarting node (saved credential; no token) → redelivers completion"
unset ENROLL_TOKEN
e2e_start_node node "e2e-node" "mock" "$WORK" "$REPOS" "$NODE_DATA"
e2e_wait_nodes_online 1 || exit 1

echo "  polling task for terminal status (timeout 40s)"
if e2e_wait_terminal 60 "$TID"; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "succeeded" ] || { echo "  A FAILED: expected succeeded, got $STATUS"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1; }
echo "  A OK: completion survived kill -9 + CP outage"

echo ">> Scenario B: network disconnect (node alive) + event continuity"

echo "  submitting task spam+sleep+spam (200 events)"
TID=$(e2e_submit $'spam:100\nsleep:3\nspam:100' "mock" "*" 60)
echo "  task $TID; waiting 1s for first 100 events to flush"
sleep 1

echo "  stopping CP for 4s (node spools second batch + completion)"
e2e_stop_cp
sleep 4

echo "  restarting CP (node alive → flush loop redelivers from outbox)"
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
e2e_login

echo "  polling task for terminal status (timeout 40s)"
if e2e_wait_terminal 60 "$TID"; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "succeeded" ] || { echo "  B FAILED: expected succeeded, got $STATUS"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1; }

echo "  checking event continuity (expect 200 spam lines, contiguous sequences)"
# Small grace: the node may finish draining the event tail just after the CP
# marks the task `succeeded` (the completion can overtake the last flush).
sleep 2
EVS=$(e2e_task_events "$TID")
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
TID=$(e2e_submit $'spam:100\nspam:100\nsleep:3' "mock" "*" 60)
echo "  task $TID; waiting 1s for the node to start it"
sleep 1

echo "  stopping CP for ${OUTAGE}s (node alive, spools)"
e2e_stop_cp
sleep "$OUTAGE"

echo "  restarting CP (node alive → flush loop redelivers)"
T_D0=$(date +%s)
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
T_D1=$(date +%s)
echo "  CP restart took $((T_D1 - T_D0))s"
e2e_login

echo "  polling task for terminal status (timeout 60s)"
if e2e_wait_terminal 80 "$TID"; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "succeeded" ] || { echo "  D FAILED: expected succeeded, got $STATUS"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1; }

echo "  checking event continuity (expect 200 spam, contiguous sequences)"
sleep 2
EVS=$(e2e_task_events "$TID")
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
e2e_kill_node
unset ENROLL_TOKEN
e2e_start_node node "e2e-node" "mock" "$WORK" "$REPOS" "$NODE_DATA"
e2e_wait_nodes_online 1 || exit 1

echo "  submitting task sleep:8 (long, will be killed)"
TID=$(e2e_submit "sleep:8" "mock" "*" 60)
echo "  task $TID; polling for running (timeout 15s)"
e2e_wait_status running "$TID" 15 || { echo "  C FAILED: never reached running before kill, got $STATUS"; exit 1; }

echo "  kill -9 node (attempt should go lost → task failed/node_lost)"
e2e_kill_node

echo "  polling for failed/lost (maintenance marks node offline after ~30s; timeout 50s)"
# Override wait_terminal's terminal set: we want failed-or-lost here.
for _ in $(seq 1 50); do
  e2e_poll_status "$TID"
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
e2e_start_node node "e2e-node" "mock" "$WORK" "$REPOS" "$NODE_DATA"
e2e_wait_nodes_online 1 || exit 1

echo "  polling task for terminal status (timeout 40s)"
if e2e_wait_terminal 60 "$TID"; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "succeeded" ] || { echo "  C FAILED: expected succeeded after retry, got $STATUS"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1; }
echo "  C OK: lost attempt → retry → succeeded"

echo ">> Scenario E: cancel mid-running → attempt cancelled (no leak)"

# Earlier scenarios left a node running; restart it so the cancel test starts
# from a clean beat (no stale attempts).
e2e_kill_node
[ -n "$CP_PID" ] || { echo "  E FAILED: CP not running"; exit 1; }
unset ENROLL_TOKEN
e2e_start_node node "e2e-node" "mock" "$WORK" "$REPOS" "$NODE_DATA"
e2e_wait_nodes_online 1 || exit 1

echo "  submitting task sleep:30 (blocks one prompt turn)"
TID=$(e2e_submit "sleep:30" "mock" "*" 120)
echo "  task $TID; polling for running (timeout 15s)"
e2e_wait_status running "$TID" 15 || { echo "  E FAILED: never reached running before cancel, got $STATUS"; exit 1; }

echo "  requesting cancellation via POST /v1/tasks/$TID/cancel"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/tasks/$TID/cancel" \
  -H "authorization: Bearer $jwt")
[ "$code" = "200" ] || { echo "  E FAILED: cancel returned $code"; exit 1; }

echo "  polling task for terminal status (timeout 30s); expect cancelled"
if e2e_wait_terminal 30 "$TID"; then :; else
  echo "  E FAILED: never reached terminal, got $STATUS"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "cancelled" ] || { echo "  E FAILED: expected cancelled, got $STATUS"; cat "$TMP/node.log"; exit 1; }
echo "  E OK: cancel aborted prompt turn, task cancelled"

echo ">> E2E OK (all scenarios)"
