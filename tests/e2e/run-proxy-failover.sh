#!/usr/bin/env bash
# Egress proxy failover E2E (ADR 0012, no Docker).
#
# Scenario: CP registered with two global proxies A and B. A node (poll
# transport) picks them up, routes its CP traffic through A, then A is
# killed — the next poll/heartbeat must rotate to B.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"
PY="$ROOT/tests/e2e/mini-forward-proxy.py"

PORT="${AGENTGRID_PORT:-7815}"
BASE="http://127.0.0.1:$PORT"
PROXY_A_PORT=18801
PROXY_B_PORT=18802
USER="admin"
PASS="changeme"
source "$ROOT/tests/e2e/lib-bootstrap.sh"

TMP="$(mktemp -d -t ag-e2e-proxy-XXXXXX)"

PIDS=()
cleanup() {
  set +e
  for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

AGENTGRID_LISTEN="127.0.0.1:$PORT" \
AGENTGRID_DB="$TMP/cp.db" \
AGENTGRID_JWT_SECRET="e2e-proxy-secret" \
AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
PIDS+=($!)

for _ in $(seq 1 40); do
  curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fsS "$BASE/health/ready" >/dev/null || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"

jwt=$(curl -fsS -X POST "$BASE/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

# Two working forward proxies, each logging hits.
python3 "$PY" $PROXY_A_PORT "$TMP/hitsA.log" & PIDS+=($!)
python3 "$PY" $PROXY_B_PORT "$TMP/hitsB.log" & PIDS+=($!)
sleep 0.5

for url in "http://127.0.0.1:$PROXY_A_PORT" "http://127.0.0.1:$PROXY_B_PORT"; do
  curl -fsS -X POST "$BASE/v1/proxies" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"url\":\"$url\"}" >/dev/null
done

ENROLL_TOKEN=$(curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
  -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

env PATH="$BIN:$PATH" \
  AGENTGRID_SERVER="$BASE" \
  AGENTGRID_DATA_DIR="$TMP/node" \
  AGENTGRID_NODE_NAME="e2e-proxy-node" \
  AGENTGRID_WORKSPACE_ROOT="$TMP/work" \
  AGENTGRID_REPOSITORY_ROOT="$TMP/repos" \
  AGENTGRID_ADAPTERS="mock" \
  AGENTGRID_TRANSPORT="poll" \
  AGENTGRID_MAX_CONCURRENCY="1" \
  AGENTGRID_PROXY_PROBE_SECS="2" \
  AGENTGRID_ENROLL_TOKEN="$ENROLL_TOKEN" \
  RUST_LOG="info" \
  nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
PIDS+=($!)

wait_hits() { # <file> — wait until the proxy logged at least one request
  local f="$1" i
  for i in $(seq 1 30); do
    [ -s "$f" ] && return 0
    sleep 1
  done
  return 1
}

echo ">> waiting for first poll via proxy A"
wait_hits "$TMP/hitsA.log" || { echo "proxy A saw no traffic"; tail -30 "$TMP/node.log"; exit 1; }
echo "   proxy A carries node traffic"

echo ">> killing proxy A — node must rotate to B"
# kill only A's python: tracked by port in argv
PA_PID=$(pgrep -f "mini-forward-proxy.py $PROXY_A_PORT" | head -1)
kill -9 "$PA_PID"
wait_hits "$TMP/hitsB.log" || { echo "FAIL: node never rotated to proxy B"; tail -30 "$TMP/node.log"; exit 1; }
echo "   rotated to proxy B"

# Node still online (heartbeats also rotated).
for _ in $(seq 1 30); do
  st=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st="none"
  [ "$st" = "online" ] && break
  sleep 1
done
[ "$st" = "online" ] || { echo "node not online after rotation (status=$st)"; exit 1; }

echo ">> reviving proxy A — prober must bring the node back"
na_before=$(wc -l < "$TMP/hitsA.log")
revived=0
for _ in $(seq 1 45); do
  if pgrep -f "mini-forward-proxy.py $PROXY_A_PORT" >/dev/null ||      { python3 "$PY" $PROXY_A_PORT "$TMP/hitsA.log" & PIDS+=($!); sleep 1; true; }; then :; fi
  na_now=$(wc -l < "$TMP/hitsA.log")
  if [ "$na_now" -gt "$na_before" ]; then revived=1; break; fi
  sleep 1
done
[ "$revived" = "1" ] || { echo "FAIL: node never returned to revived proxy A"; tail -20 "$TMP/node.log"; exit 1; }
echo "   node returned to revived proxy A"

echo "PROXY FAILOVER E2E OK"
