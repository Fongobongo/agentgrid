#!/usr/bin/env bash
# Plan 0.3 stage 0: run the load harness (crates/control-plane/tests/load.rs)
# and print the LOAD-RESULT summary. The harness is in-process (real HTTP
# server + N mock node clients), so nothing external needs teardown.
#
# Knobs: AG_LOAD_NODES (50), AG_LOAD_TASKS (500), AG_LOAD_POLL_MS (1000),
# AG_LOAD_TRANSPORT (poll; set `ws` for the WS-push variant — assert p99 < 200 ms).
# AG_LOAD_IDLE=1 runs the Plan 6.5 idle-fleet mode instead (100 idle nodes
# hammering heartbeat+poll; AG_LOAD_IDLE_NODES/AG_LOAD_IDLE_ROUNDS).
# Usage: tests/e2e/run-load.sh
set -euo pipefail

cd "$(dirname "$0")/../.."

if [ "${AG_LOAD_IDLE:-0}" = "1" ]; then
  NODES="${AG_LOAD_IDLE_NODES:-100}"
  ROUNDS="${AG_LOAD_IDLE_ROUNDS:-10}"
  echo ">> idle fleet load: nodes=$NODES rounds=$ROUNDS (heartbeat+poll, zero tasks)"
  out=$(AG_LOAD_IDLE_NODES="$NODES" AG_LOAD_IDLE_ROUNDS="$ROUNDS" \
    CARGO_INCREMENTAL=0 cargo test -p agentgrid-control-plane --test load -- \
    --ignored --nocapture idle_nodes_heartbeat_poll_load 2>&1)
  rc=$?
  echo "$out" | grep -E "IDLE-RESULT|panicked|assert" || true
  if [ $rc -ne 0 ]; then
    echo "IDLE LOAD FAILED (rc=$rc)"
    echo "$out" | tail -30
    exit 1
  fi
  rm -f /var/tmp/ag-test-*.db* 2>/dev/null || true
  echo "IDLE LOAD OK"
  exit 0
fi

NODES="${AG_LOAD_NODES:-50}"
TASKS="${AG_LOAD_TASKS:-500}"
POLL_MS="${AG_LOAD_POLL_MS:-1000}"
TRANSPORT="${AG_LOAD_TRANSPORT:-poll}"

echo ">> load harness: nodes=$NODES tasks=$TASKS poll=${POLL_MS}ms transport=$TRANSPORT"
out=$(AG_LOAD_NODES="$NODES" AG_LOAD_TASKS="$TASKS" AG_LOAD_POLL_MS="$POLL_MS" \
  AG_LOAD_TRANSPORT="$TRANSPORT" \
  CARGO_INCREMENTAL=0 cargo test -p agentgrid-control-plane --test load -- \
  --ignored --nocapture load_baseline_mock_nodes 2>&1)
rc=$?
echo "$out" | grep -E "LOAD-RESULT|panicked|assert" || true
if [ $rc -ne 0 ]; then
  echo "LOAD FAILED (rc=$rc)"
  echo "$out" | tail -30
  exit 1
fi
# Clean the harness DBs left in /var/tmp (AppState::open_temp hygiene is via
# Drop only while the state lives; the ignored test drops normally).
rm -f /var/tmp/ag-test-*.db* 2>/dev/null || true
echo "LOAD OK"
