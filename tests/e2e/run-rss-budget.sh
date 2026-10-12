#!/usr/bin/env bash
# RSS budget E2E (AGENTS.md "Testing" budgets). Process-based, no Docker.
#
# Asserts three budgets, each overridable via env:
#   node daemon idle            AG_RSS_NODE_IDLE_MIB   (default 25)
#   control plane (after load)  AG_RSS_CP_MIB          (default 64)
#   node streaming a task       AG_RSS_NODE_STREAM_MIB (default 60)
#
# Note: debug builds carry more allocator overhead than release musl
# binaries; run against `cargo build --release` binaries for the numbers
# that matter. Exit 1 on any budget violation.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="${AG_BIN_DIR:-$ROOT/target/debug}"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7821}"
PORT="${AGENTGRID_PORT:-7821}"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-rss-secret"
E2E_CONCURRENCY=2

NODE_IDLE_BUDGET="${AG_RSS_NODE_IDLE_MIB:-25}"
CP_BUDGET="${AG_RSS_CP_MIB:-64}"
NODE_STREAM_BUDGET="${AG_RSS_NODE_STREAM_MIB:-60}"

TMP="$(mktemp -d -t ag-e2e-rss-XXXXXX)"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

rss_mib() { awk '/VmRSS/{printf "%d", $2/1024}' "/proc/$1/status" 2>/dev/null || echo 0; }

e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node rss "e2e-rss-node" "mock" "$TMP/work" "$TMP/repos" "$TMP/node"
e2e_wait_nodes_online 1 || exit 1

sleep 2
NODE_IDLE=$(rss_mib "$NODE_PID")
echo "node idle RSS: ${NODE_IDLE} MiB (budget ${NODE_IDLE_BUDGET})"

# Streaming phase: a mock task emitting output while we sample the node.
TID=$(e2e_submit $'spam:2000\nsleep:15' "mock" "*" 120)
NODE_PEAK=0
for _ in $(seq 1 90); do
  cur=$(rss_mib "$NODE_PID"); [ "$cur" -gt "$NODE_PEAK" ] && NODE_PEAK=$cur
  e2e_poll_status "$TID"
  [ "$STATUS" = "succeeded" ] && break
  [ "$STATUS" = "failed" ] && { echo "task failed"; cat "$TMP/rss.log"; exit 1; }
  sleep 1
done
e2e_poll_status "$TID"
[ "$STATUS" = "succeeded" ] || { echo "task never succeeded"; exit 1; }
CP_PEAK=$(rss_mib "$CP_PID")
echo "node streaming peak RSS: ${NODE_PEAK} MiB (budget ${NODE_STREAM_BUDGET})"
echo "control plane RSS: ${CP_PEAK} MiB (budget ${CP_BUDGET})"

fail=0
[ "$NODE_IDLE" -le "$NODE_IDLE_BUDGET" ] || { echo "BUDGET: node idle"; fail=1; }
[ "$NODE_PEAK" -le "$NODE_STREAM_BUDGET" ] || { echo "BUDGET: node streaming"; fail=1; }
[ "$CP_PEAK" -le "$CP_BUDGET" ] || { echo "BUDGET: control plane"; fail=1; }
[ "$fail" = "0" ] && echo "RSS budgets OK"
exit "$fail"
