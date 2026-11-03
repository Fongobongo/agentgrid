#!/usr/bin/env bash
# Egress proxy pool E2E (ADR 0012, no Docker).
#
# Two transports, two things to prove, because they are not the same thing:
#
#   poll  — the pool arrives with the poll response, the node's traffic goes
#           through proxy A, and killing A rotates it to B. This is the
#           original scenario and it exercises the data plane.
#
#   ws    — the default transport (`auto` prefers WS) and the one where the
#           pool used to arrive **never**: it travelled only in a poll
#           response, and a ws node never polls. Its own upgrade can only be
#           direct (the pool arrives inside the session it would have to
#           proxy), so the traffic assertion cannot apply here — what is
#           asserted instead is the delivery itself, in the daemon's own log,
#           for the connect push and for both write paths (add, remove) down
#           to the empty pool that means "the operator deleted them all".
#
# Run with AGENTGRID_E2E_TRANSPORT=poll for the polling leg.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"
PY="$ROOT/tests/e2e/mini-forward-proxy.py"

TRANSPORT="${AGENTGRID_E2E_TRANSPORT:-ws}"
PORT="${AGENTGRID_PORT:-7815}"
BASE="http://127.0.0.1:$PORT"
PROXY_A_PORT=18801
PROXY_B_PORT=18802
USER="admin"
PASS="dev-only-password"
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

start_cp() {
  AGENTGRID_LISTEN="127.0.0.1:$PORT" \
  AGENTGRID_DB="$TMP/cp.db" \
  AGENTGRID_JWT_SECRET="e2e-proxy-secret-0123456789abcdef" \
  AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
  nohup "$BIN/agentgrid-control-plane" >>"$TMP/cp.log" 2>&1 &
  PIDS+=($!)
  for _ in $(seq 1 40); do
    curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  echo "CP not ready"
  tail -30 "$TMP/cp.log"
  exit 1
}

start_cp
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"

jwt=$(curl -fsS -X POST "$BASE/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

add_proxy() { # <url> → prints the id
  curl -fsS -X POST "$BASE/v1/proxies" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"url\":\"$1\"}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}

rm_proxy() { # <id>
  curl -fsS -X DELETE "$BASE/v1/proxies/$1" -H "authorization: Bearer $jwt" >/dev/null
}

# Two working forward proxies, each logging hits. PIDs are kept by hand:
# `pgrep -f` is not available in git-bash, and a port-scraped PID is exactly
# the kind of lookup that silently matches nothing and turns the kill into a
# no-op — which would make the rotation assertion pass for the wrong reason.
python3 "$PY" $PROXY_A_PORT "$TMP/hitsA.log" & PA_PID=$!; PIDS+=($PA_PID)
python3 "$PY" $PROXY_B_PORT "$TMP/hitsB.log" & PIDS+=($!)
sleep 0.5

ID_A=$(add_proxy "http://127.0.0.1:$PROXY_A_PORT")
ID_B=$(add_proxy "http://127.0.0.1:$PROXY_B_PORT")

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
  AGENTGRID_TRANSPORT="$TRANSPORT" \
  AGENTGRID_MAX_CONCURRENCY="1" \
  AGENTGRID_PROXY_PROBE_SECS="2" \
  AGENTGRID_ENROLL_TOKEN="$ENROLL_TOKEN" \
  RUST_LOG="info" \
  nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
PIDS+=($!)

wait_hits() { # <file> [secs] — wait until the proxy logged at least one request
  local f="$1" secs="${2:-60}" i
  for i in $(seq 1 "$secs"); do
    [ -s "$f" ] && return 0
    sleep 1
  done
  return 1
}

wait_pool() { # <urls> <complete> [secs] — the daemon logged this delivery
  local urls="$1" complete="$2" secs="${3:-40}" i
  for i in $(seq 1 "$secs"); do
    grep -q "\"urls\":$urls,\"complete\":$complete" "$TMP/node.log" && return 0
    sleep 1
  done
  return 1
}

node_status() {
  curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["status"] if ns else "none")' 2>/dev/null || echo none
}

echo ">> transport=$TRANSPORT"

if [ "$TRANSPORT" = "ws" ]; then
  # Delivery, not routing: a ws node learns its pool from the session it
  # already opened, so its first upgrade is necessarily direct.
  echo ">> waiting for the connect push (2 urls)"
  wait_pool 2 true || { echo "FAIL: the pool never reached the ws node"; tail -20 "$TMP/node.log"; exit 1; }
  echo "   connect push delivered the whole pool"

  echo ">> removing proxy B — the CP must push the shortened pool"
  rm_proxy "$ID_B"
  wait_pool 1 true || { echo "FAIL: no push after removing one proxy"; tail -20 "$TMP/node.log"; exit 1; }
  echo "   write push delivered 1 url"

  echo ">> removing proxy A — an empty pool must be marked complete"
  rm_proxy "$ID_A"
  wait_pool 0 true || { echo "FAIL: removing the last proxy produced no push"; tail -20 "$TMP/node.log"; exit 1; }
  echo "   empty + complete reached the node (revocation is representable)"

  st=$(node_status)
  [ "$st" = "online" ] || { echo "FAIL: node went offline (status=$st)"; tail -20 "$TMP/node.log"; exit 1; }
  echo "PROXY POOL E2E OK (transport=ws)"
  exit 0
fi

echo ">> waiting for the first request via proxy A"
wait_hits "$TMP/hitsA.log" 60 || { echo "proxy A saw no traffic"; tail -30 "$TMP/node.log"; exit 1; }
echo "   proxy A carries node traffic"

echo ">> killing proxy A — node must rotate to B"
kill -9 "$PA_PID"
wait_hits "$TMP/hitsB.log" 60 || { echo "FAIL: node never rotated to proxy B"; tail -30 "$TMP/node.log"; exit 1; }
echo "   rotated to proxy B"

# Node still online (heartbeats also rotated).
for _ in $(seq 1 30); do
  st=$(node_status)
  [ "$st" = "online" ] && break
  sleep 1
done
[ "$st" = "online" ] || { echo "node not online after rotation (status=$st)"; exit 1; }

echo ">> reviving proxy A — prober must bring the node back"
na_before=$(wc -l < "$TMP/hitsA.log")
revived=0
for _ in $(seq 1 45); do
  if ! kill -0 "$PA_PID" 2>/dev/null; then
    python3 "$PY" $PROXY_A_PORT "$TMP/hitsA.log" & PA_PID=$!; PIDS+=($PA_PID); sleep 1
  fi
  na_now=$(wc -l < "$TMP/hitsA.log")
  if [ "$na_now" -gt "$na_before" ]; then revived=1; break; fi
  sleep 1
done
[ "$revived" = "1" ] || { echo "FAIL: node never returned to revived proxy A"; tail -20 "$TMP/node.log"; exit 1; }
echo "   node returned to revived proxy A"

echo "PROXY FAILOVER E2E OK (transport=poll)"
