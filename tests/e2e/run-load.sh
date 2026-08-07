#!/usr/bin/env bash
# Plan 0.3 stage 0: run the load harness (crates/control-plane/tests/load.rs)
# and print the LOAD-RESULT summary. The harness is in-process (real HTTP
# server + N mock node clients), so nothing external needs teardown.
#
# Knobs: AG_LOAD_NODES (50), AG_LOAD_TASKS (500), AG_LOAD_POLL_MS (1000).
# Usage: tests/e2e/run-load.sh
set -euo pipefail

cd "$(dirname "$0")/../.."

NODES="${AG_LOAD_NODES:-50}"
TASKS="${AG_LOAD_TASKS:-500}"
POLL_MS="${AG_LOAD_POLL_MS:-1000}"

echo ">> load harness: nodes=$NODES tasks=$TASKS poll=${POLL_MS}ms"
out=$(AG_LOAD_NODES="$NODES" AG_LOAD_TASKS="$TASKS" AG_LOAD_POLL_MS="$POLL_MS" \
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
