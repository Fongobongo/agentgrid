#!/usr/bin/env bash
# RSS budget bench (Plan 6.3 #532 / 6.14 acceptance): idle control-plane RSS
# ≤ 64 MiB, idle node RSS ≤ 25 MiB, streaming node RSS ≤ 60 MiB (child
# process excluded — item 534 reports the child peak separately per
# attempt). Process-based CP+node, mock adapter, plain-dir tasks.
#
# Env:
#   BIN (default target/debug) — binaries under test (debug or release).
#   AG_RSS_MODE=assert|report (default report): assert fails the run when a
#     budget is exceeded (release pipeline); report only prints.
#   AG_RSS_CP_BUDGET_MB / AG_RSS_NODE_BUDGET_MB / AG_RSS_STREAM_BUDGET_MB
#     (defaults 64 / 25 / 60).
# Requires: linux (/proc for VmRSS), curl, python3. Skips cleanly (exit 0)
# elsewhere — RSS numbers are only meaningful on a Tier-1 Linux host.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="${BIN:-$ROOT/target/debug}"

[ "$(uname -s)" = "Linux" ] || { echo ">> skip: not linux (no /proc VmRSS)"; exit 0; }
[ -d /proc/1 ] || { echo ">> skip: no /proc"; exit 0; }
for b in agentgrid-control-plane agentgrid-node-daemon adapter-mock; do
  [ -x "$BIN/$b" ] || { echo ">> skip: $BIN/$b not built"; exit 0; }
done

MODE="${AG_RSS_MODE:-report}"
CP_BUDGET="${AG_RSS_CP_BUDGET_MB:-64}"
NODE_BUDGET="${AG_RSS_NODE_BUDGET_MB:-25}"
STREAM_BUDGET="${AG_RSS_STREAM_BUDGET_MB:-60}"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7817}"
PORT="${AGENTGRID_PORT:-7817}"
USER="admin"
source "$ROOT/tests/e2e/lib-bootstrap.sh"
PASS="changeme"

TMP="$(mktemp -d -t ag-e2e-rss-XXXXXX)"
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
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

# VmRSS in KiB for a pid (0 when the process is gone).
rss_kb() {
  awk '/^VmRSS/{print $2}' "/proc/$1/status" 2>/dev/null || echo 0
}

# Max VmRSS over N samples spaced SLEEP_S apart.
max_rss_kb() {  # $1 = pid, $2 = samples, $3 = sleep_s
  local pid="$1" max=0 v
  for _ in $(seq 1 "$2"); do
    v=$(rss_kb "$pid")
    [ "${v:-0}" -gt "$max" ] 2>/dev/null && max="$v"
    sleep "$3"
  done
  echo "$max"
}

check() {  # $1 = label, $2 = kb, $3 = budget_mb
  local mb=$(( $2 / 1024 ))
  if [ "$MODE" = "assert" ] && [ "$mb" -gt "$3" ]; then
    echo "  FAIL: $1 = ${mb} MiB > budget ${3} MiB"
    return 1
  fi
  echo "  ok: $1 = ${mb} MiB (budget ${3} MiB)"
  return 0
}

echo ">> RSS bench: mode=$MODE binaries=$BIN"
AGENTGRID_LISTEN="127.0.0.1:$PORT" \
AGENTGRID_DB="$CP_DB" \
AGENTGRID_JWT_SECRET="e2e-stable-secret" \
AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
CP_PID=$!
for _ in $(seq 1 40); do
  curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fsS "$BASE/health/ready" >/dev/null || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }

echo ">> CP idle RSS (settling 10s, then 3 samples)"
sleep 10
CP_KB=$(max_rss_kb "$CP_PID" 3 2)

bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
jwt=$(curl -fsS -X POST "$BASE/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
ENROLL_TOKEN=$(curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
  -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

env PATH="$BIN:$PATH" \
  AGENTGRID_SERVER="$BASE" \
  AGENTGRID_DATA_DIR="$NODE_DATA" \
  AGENTGRID_NODE_NAME="e2e-rss" \
  AGENTGRID_WORKSPACE_ROOT="$WORK" \
  AGENTGRID_REPOSITORY_ROOT="$REPOS" \
  AGENTGRID_ADAPTERS="mock" \
  AGENTGRID_MAX_CONCURRENCY="1" \
  AGENTGRID_ENROLL_TOKEN="$ENROLL_TOKEN" \
  RUST_LOG="info" \
  nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
NODE_PID=$!
for _ in $(seq 1 60); do
  st=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" 2>/dev/null \
    | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st="none"
  [ "$st" = "online" ] && break
  sleep 0.5
done
[ "$st" = "online" ] || { echo "node never online"; cat "$TMP/node.log"; exit 1; }

echo ">> node idle RSS (settling 10s, then 3 samples)"
sleep 10
NODE_KB=$(max_rss_kb "$NODE_PID" 3 2)

echo ">> node streaming RSS (spam:3000 task, sampling every 0.2s to terminal)"
TID=$(python3 -c 'import json,sys;print(json.dumps("spam:3000"))' | {
  read -r prompt_json
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$prompt_json,\"repository\":\"*\",\"adapter\":\"mock\",\"timeout_secs\":120}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
})
STREAM_KB=0
for _ in $(seq 1 600); do
  v=$(rss_kb "$NODE_PID")
  [ "${v:-0}" -gt "$STREAM_KB" ] 2>/dev/null && STREAM_KB="$v"
  STATUS=$(curl -fsS "$BASE/v1/tasks/$TID" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
  case "$STATUS" in
    succeeded|failed|cancelled|timed_out|lost) break;;
  esac
  sleep 0.2
done
[ "$STATUS" = "succeeded" ] || { echo ">> FAILED: streaming task ended $STATUS"; exit 1; }

echo ">> RSS results (mode=$MODE)"
rc=0
check "control-plane idle" "$CP_KB" "$CP_BUDGET" || rc=1
check "node idle" "$NODE_KB" "$NODE_BUDGET" || rc=1
check "node streaming" "$STREAM_KB" "$STREAM_BUDGET" || rc=1
[ "$rc" = "0" ] && echo ">> RSS bench OK"
exit "$rc"
