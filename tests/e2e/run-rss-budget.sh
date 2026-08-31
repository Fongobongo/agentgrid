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
source "$ROOT/tests/e2e/lib-bootstrap.sh"

NODE_IDLE_BUDGET="${AG_RSS_NODE_IDLE_MIB:-25}"
CP_BUDGET="${AG_RSS_CP_MIB:-64}"
NODE_STREAM_BUDGET="${AG_RSS_NODE_STREAM_MIB:-60}"

TMP="$(mktemp -d -t ag-e2e-rss-XXXXXX)"
CP_PID=""; NODE_PID=""
cleanup() {
  set +e
  [ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null
  [ -n "$CP_PID" ] && kill "$CP_PID" 2>/dev/null
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

rss_mib() { awk '/VmRSS/{printf "%d", $2/1024}' "/proc/$1/status" 2>/dev/null || echo 0; }

AGENTGRID_LISTEN="127.0.0.1:$PORT" \
AGENTGRID_DB="$TMP/cp.db" \
AGENTGRID_JWT_SECRET="e2e-rss-secret" \
AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
CP_PID=$!
for _ in $(seq 1 40); do
  curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fsS "$BASE/health/ready" >/dev/null || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"

jwt=$(curl -fsS -X POST "$BASE/v1/auth/login" -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

ENROLL_TOKEN=$(curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
  -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos"
env PATH="$BIN:$PATH" \
  AGENTGRID_SERVER="$BASE" \
  AGENTGRID_DATA_DIR="$TMP/node" \
  AGENTGRID_NODE_NAME="e2e-rss-node" \
  AGENTGRID_WORKSPACE_ROOT="$TMP/work" \
  AGENTGRID_REPOSITORY_ROOT="$TMP/repos" \
  AGENTGRID_ADAPTERS="mock" \
  AGENTGRID_MAX_CONCURRENCY="2" \
  AGENTGRID_ENROLL_TOKEN="$ENROLL_TOKEN" \
  nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
NODE_PID=$!

for _ in $(seq 1 60); do
  st=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st="none"
  [ "$st" = "online" ] && break
  sleep 0.5
done
[ "$st" = "online" ] || { echo "node never came online"; cat "$TMP/node.log"; exit 1; }

sleep 2
NODE_IDLE=$(rss_mib "$NODE_PID")
echo "node idle RSS: ${NODE_IDLE} MiB (budget ${NODE_IDLE_BUDGET})"

# Streaming phase: a mock task emitting output while we sample the node.
TID=$(curl -fsS -X POST "$BASE/v1/tasks" -H "authorization: Bearer $jwt" \
  -H 'content-type: application/json' \
  -d '{"prompt":"spam:2000\nsleep:15","repository":"*","adapter":"mock","timeout_secs":120}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
NODE_PEAK=0
for _ in $(seq 1 90); do
  cur=$(rss_mib "$NODE_PID"); [ "$cur" -gt "$NODE_PEAK" ] && NODE_PEAK=$cur
  st=$(curl -fsS "$BASE/v1/tasks/$TID" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
  [ "$st" = "succeeded" ] && break
  [ "$st" = "failed" ] && { echo "task failed"; cat "$TMP/node.log"; exit 1; }
  sleep 1
done
[ "$(curl -fsS "$BASE/v1/tasks/$TID" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')" = "succeeded" ] \
  || { echo "task never succeeded"; exit 1; }
CP_PEAK=$(rss_mib "$CP_PID")
echo "node streaming peak RSS: ${NODE_PEAK} MiB (budget ${NODE_STREAM_BUDGET})"
echo "control plane RSS: ${CP_PEAK} MiB (budget ${CP_BUDGET})"

fail=0
[ "$NODE_IDLE" -le "$NODE_IDLE_BUDGET" ] || { echo "BUDGET: node idle"; fail=1; }
[ "$NODE_PEAK" -le "$NODE_STREAM_BUDGET" ] || { echo "BUDGET: node streaming"; fail=1; }
[ "$CP_PEAK" -le "$CP_BUDGET" ] || { echo "BUDGET: control plane"; fail=1; }
[ "$fail" = 0 ] && echo "RSS budgets OK"
exit "$fail"
