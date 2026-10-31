#!/usr/bin/env bash
# E2E (process-based, no Docker, no LLM): the WebSocket transport through an
# egress proxy.
#
# ADR 0012 shipped a proxy pool for the node's HTTP egress, and ADR 0026 had to
# document its one blind spot: tokio-tungstenite has no proxy support, so a
# proxied node silently degraded to polling — and with `AGENTGRID_TRANSPORT=ws`
# (no fallback) it failed outright. The node now dials the control plane through
# an HTTP `CONNECT` tunnel and hands that socket to tungstenite, which does the
# upgrade inside it.
#
# What is asserted, and why each one is needed:
#   1. a node with `AGENTGRID_TRANSPORT=ws` and `AGENTGRID_PROXY_URLS` pointed at
#      a CONNECT-capable proxy comes online — with `ws` there is no fallback, so
#      "online" already means the WS channel established;
#   2. the node log says so (`ws control channel connected`), i.e. it did not
#      quietly end up on polling;
#   3. the proxy's hit log contains `CONNECT <cp-addr>` — the discriminating
#      assertion. Everything above would also pass if the node had dialled the
#      control plane directly and ignored the proxy; only the proxy's own record
#      proves the traffic went through the tunnel;
#   4. a task submitted after the session is up completes, so the control
#      channel carried a real assignment, not just a handshake.
#
# Honest limit: everything is on 127.0.0.1, so "must use the proxy" is expressed
# by configuration plus assertion 3 rather than by a firewall blocking the
# direct route.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="${AGENTGRID_BIN:-$ROOT/target/debug}"
PROXY_PY="$ROOT/tests/e2e/mini-forward-proxy.py"

PORT="${AGENTGRID_PORT:-7850}"
BASE="http://127.0.0.1:$PORT"
PROXY_PORT="${AGENTGRID_PROXY_PORT:-18810}"
PROXY="http://127.0.0.1:$PROXY_PORT"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-ws-proxy-secret"
E2E_CONCURRENCY=2

# A dev box may export HTTP_PROXY (this one does). The node's tunnel dials the
# proxy address itself, but keep curl and the CP off any ambient proxy.
export NO_PROXY="127.0.0.1,localhost"
export no_proxy="127.0.0.1,localhost"

TMP="$(mktemp -d -t ag-e2e-ws-proxy-XXXXXX)"
mkdir -p "$TMP/work" "$TMP/repos" "$TMP/node"
HITS="$TMP/proxy-hits.log"
: >"$HITS"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

json_str() { python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1"; }

echo ">> ws-through-proxy E2E: CONNECT tunnel, then the upgrade inside it"

e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token

# --- 1. a CONNECT-capable proxy in front of the control plane -------------
python3 "$PROXY_PY" "$PROXY_PORT" "$HITS" >"$TMP/proxy.log" 2>&1 &
PROXY_PID=$!
E2E_PIDS="$E2E_PIDS $PROXY_PID"
for _ in $(seq 1 40); do
  if python3 - "$PROXY_PORT" <<'PY'
import socket, sys
try:
    socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=0.5).close()
except OSError:
    raise SystemExit(1)
PY
  then break; fi
  sleep 0.1
done
echo "   proxy listening on 127.0.0.1:$PROXY_PORT"

# --- 2. a node on the ws transport, routed through it --------------------
echo ">> node on AGENTGRID_TRANSPORT=ws with AGENTGRID_PROXY_URLS=$PROXY"
e2e_start_node n1 "ws-proxy-n1" "mock" "$TMP/work" "$TMP/repos" "$TMP/node" \
  AGENTGRID_TRANSPORT=ws \
  AGENTGRID_PROXY_URLS="$PROXY"
e2e_wait_nodes_online 1 || { echo "node never came online over the proxy"; tail -40 "$TMP/n1.log"; exit 1; }

# --- 3. it is the WS channel, not a quiet fallback ----------------------
grep -q "ws control channel connected" "$TMP/n1.log" \
  || { echo "the node is online but never established the ws channel:"; tail -40 "$TMP/n1.log"; exit 1; }
echo "   node is online on the ws channel"

# --- 4. the proxy's own record: the tunnel was used ---------------------
grep -q "CONNECT 127.0.0.1:$PORT" "$HITS" \
  || {
    echo "the proxy never saw a CONNECT to 127.0.0.1:$PORT — the node dialled the"
    echo "control plane directly and the proxy was decoration. Hit log:"
    cat "$HITS"
    exit 1
  }
echo "   proxy recorded: $(grep -c "CONNECT 127.0.0.1:$PORT" "$HITS") CONNECT(s) to the control plane"

# --- 5. the channel carries a real assignment ---------------------------
echo ">> a task after the tunnelled session is up"
TID=$(curl -fsS -X POST "$BASE/v1/tasks" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d "{\"prompt\":$(json_str "write:through-the-tunnel.txt:hi"),\"repository\":\"*\",\"adapter\":\"mock\",\"timeout_secs\":120}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
e2e_wait_terminal 120 "$TID" || { echo "task never finished"; tail -40 "$TMP/n1.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "task status=$STATUS"; tail -40 "$TMP/n1.log"; exit 1; }
echo "   task $TID: $STATUS"

echo "OK: the ws transport ran through an HTTP CONNECT proxy"
