#!/usr/bin/env bash
# E2E (process-based, no Docker, no LLM): a node carried by a peer's relay.
#
# The topology this proves: node B cannot reach the control plane, but it can
# reach node A, and A can reach the control plane. A runs a relay
# (`AGENTGRID_RELAY_LISTEN`) and B is configured with A's address as its
# control plane — nothing else. If B comes online and runs a task, its traffic
# went through A, because B has no other address to dial.
#
# What is asserted, in the order it can break:
#   1. the allowlist is wired, not just parsed: a peer outside it gets 403;
#   2. B enrolls, comes online and completes a task *pinned to B* — through A;
#   3. the relay's counters move (it really carried the requests);
#   4. the WebSocket transport also works through the relay (the edge bridges
#      /v1/node/ws; nothing in the node had to change for this);
#   5. the relay is the only path: kill A and B starts failing its polls;
#   6. given *two* relays (`AGENTGRID_SERVERS`), killing the first does not cut
#      B off: it rotates onto the second and keeps running tasks. This is the
#      redundancy answer to "what if the middle host dies" — it is configuration,
#      not discovery, and without the list there is nothing to rotate to (5).
#   7. the other half of 6: a relay that is *alive* but has lost its own route
#      answers `502`, which no connect error ever surfaces. The node must leave
#      it — after several answers, not on the first — and get through the
#      working relay.
#
# Honest limit: every process here is on 127.0.0.1, so "B cannot reach the
# control plane" is expressed by B's configuration (it is told only the relays'
# addresses) rather than by a real firewall. That is the part a unit test cannot
# cover anyway — the wiring from `AGENTGRID_SERVER`/`AGENTGRID_SERVERS` through a
# real HTTP hop.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="${AGENTGRID_BIN:-$ROOT/target/debug}"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7830}"
PORT="${AGENTGRID_PORT:-7830}"
RELAY_PORT="${AGENTGRID_RELAY_PORT:-7832}"
RELAY="http://127.0.0.1:$RELAY_PORT"
RELAY2_PORT="${AGENTGRID_RELAY2_PORT:-7833}"
RELAY2="http://127.0.0.1:$RELAY2_PORT"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-relay-secret"
E2E_CONCURRENCY=2

# A dev box may export HTTP_PROXY (this one does). reqwest honours it, so a dead
# relay port answers `502 Bad Gateway` from the proxy instead of refusing the
# connection — and the node rotates endpoints only on a connect/timeout error, so
# it would sit on the dead relay forever and this script would blame the product.
# The premise here is direct localhost traffic, so pin it for every child (and for
# curl, which reads the same variables).
export NO_PROXY="127.0.0.1,localhost"
export no_proxy="127.0.0.1,localhost"

TMP="$(mktemp -d -t ag-e2e-relay-XXXXXX)"
for d in a b c work-a work-b work-c repos-a repos-b repos-c; do mkdir -p "$TMP/$d"; done

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

json_str() { python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1"; }

node_id_by_name() {  # $1 = node name; prints its id
  curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json
d=json.load(sys.stdin); ns=d.get("items",d) if isinstance(d,dict) else d
print(next((n["id"] for n in ns if n.get("name")==sys.argv[1]), ""))' "$1"
}

# A task pinned to one node: without the pin, either node could run it and the
# test would pass even if B were doing nothing.
submit_pinned() {  # $1 = prompt, $2 = node id
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$(json_str "$1"),\"repository\":\"*\",\"adapter\":\"mock\",\"timeout_secs\":120,\"requested_node_id\":\"$2\"}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}

metric_at() {  # $1 = relay base url, $2 = metric name; prints the value (0 if absent)
  curl -fsS "$1/metrics" 2>/dev/null \
    | awk -v m="$2" '$1==m {print $2; found=1} END{if(!found) print 0}' || echo 0
}

metric() {  # $1 = metric name on relay A; prints the value (0 if absent)
  metric_at "$RELAY" "$1"
}

echo ">> relay E2E: a node that cannot reach the control plane, carried by a peer"

e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token

# --- 1. the allowlist is a real gate --------------------------------------
echo ">> an allowlist that excludes this host must refuse it"
e2e_start_node a "relay-a" "mock" "$TMP/work-a" "$TMP/repos-a" "$TMP/a" \
  AGENTGRID_RELAY_LISTEN="127.0.0.1:$RELAY_PORT" \
  AGENTGRID_RELAY_ALLOW="10.0.0.0/8"
A_PID=$NODE_PID
e2e_wait_nodes_online 1 || { echo "node A never came online"; tail -20 "$TMP/a.log"; exit 1; }
# The daemon is online (so the relay task started) while the relay refuses us.
CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$RELAY/health/live" || true)
[ "$CODE" = "403" ] || { echo "expected 403 from the relay allowlist, got $CODE"; tail -20 "$TMP/a.log"; exit 1; }
echo "   relay answered $CODE for 127.0.0.1 under AGENTGRID_RELAY_ALLOW=10.0.0.0/8"

# --- restart A with an allowlist that admits this host ---------------------
e2e_kill_node
ENROLL_TOKEN=""
e2e_start_node a "relay-a" "mock" "$TMP/work-a" "$TMP/repos-a" "$TMP/a" \
  AGENTGRID_RELAY_LISTEN="127.0.0.1:$RELAY_PORT" \
  AGENTGRID_RELAY_ALLOW="127.0.0.1/32"
A_PID=$NODE_PID
e2e_wait_nodes_online 1 || { echo "node A did not return"; tail -20 "$TMP/a.log"; exit 1; }
curl -fsS --max-time 5 "$RELAY/health/ready" >/dev/null \
  || { echo "relay is not ready"; tail -20 "$TMP/a.log"; exit 1; }
echo "   relay ready with AGENTGRID_RELAY_ALLOW=127.0.0.1/32"

# --- 2. node B knows only the relay --------------------------------------
echo ">> node B is given the relay address and nothing else"
# A fresh token: enrollment tokens are one-shot and A already spent the first.
e2e_mint_token
e2e_start_node b "relay-b" "mock" "$TMP/work-b" "$TMP/repos-b" "$TMP/b" \
  AGENTGRID_SERVER="$RELAY" \
  AGENTGRID_TRANSPORT=poll
B_PID=$NODE_PID
e2e_wait_nodes_online 2 || { echo "B never registered through the relay"; tail -30 "$TMP/b.log"; exit 1; }

BID=$(node_id_by_name "relay-b")
[ -n "$BID" ] || { echo "relay-b is not in /v1/nodes"; exit 1; }
echo "   relay-b registered through the relay as $BID"

TID=$(submit_pinned "write:relayed.txt:hi" "$BID")
e2e_wait_terminal 120 "$TID" || { echo "pinned task never finished"; tail -30 "$TMP/b.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "task status=$STATUS"; tail -30 "$TMP/b.log"; exit 1; }
echo "   task $TID pinned to relay-b: $STATUS"

# --- 3. the relay carried it ---------------------------------------------
REQS=$(metric agentgrid_edge_requests_total)
[ "${REQS:-0}" -gt 0 ] || { echo "relay handled 0 requests — B did not use it"; exit 1; }
echo "   relay handled $REQS requests"

# --- 4. the WebSocket transport also survives the hop ---------------------
echo ">> the same node over AGENTGRID_TRANSPORT=ws"
e2e_kill_node
ENROLL_TOKEN=""
e2e_start_node b "relay-b" "mock" "$TMP/work-b" "$TMP/repos-b" "$TMP/b" \
  AGENTGRID_SERVER="$RELAY" \
  AGENTGRID_TRANSPORT=ws
B_PID=$NODE_PID
e2e_wait_nodes_online 2 || { echo "B did not come back over ws"; tail -30 "$TMP/b.log"; exit 1; }
BRIDGES=$(metric agentgrid_edge_ws_bridges_total)
[ "${BRIDGES:-0}" -gt 0 ] || { echo "no ws bridge on the relay — B is not using ws"; tail -20 "$TMP/b.log"; exit 1; }
echo "   relay bridged $BRIDGES websocket(s)"

TID=$(submit_pinned "write:relayed-ws.txt:hi" "$BID")
e2e_wait_terminal 120 "$TID" || { echo "ws task never finished"; tail -30 "$TMP/b.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "ws task status=$STATUS"; tail -30 "$TMP/b.log"; exit 1; }
echo "   task $TID pinned to relay-b over ws: $STATUS"

# --- 5. the relay is the only path ---------------------------------------
echo ">> killing the relay must cut B off (there is no second route)"
kill -9 "$A_PID" 2>/dev/null || true
wait "$A_PID" 2>/dev/null || true
e2e_forget_pid "$A_PID"
for _ in $(seq 1 40); do
  if grep -q "poll failed\|ws control channel\|reconnect" "$TMP/b.log" 2>/dev/null; then
    break
  fi
  sleep 0.5
done
grep -qi "poll failed\|websocket\|reconnect\|transport" "$TMP/b.log" \
  || { echo "B logged no transport failure after the relay died"; tail -30 "$TMP/b.log"; exit 1; }
echo "   B reports the loss of its only route"

# --- 6. two relays: the second one keeps B connected ----------------------
# The question this answers: "if the middle host dies but another node could
# carry the traffic, does it happen by itself?" Not by itself — the node has no
# discovery — but `AGENTGRID_SERVERS` lists the hops and the existing endpoint
# rotation moves onto the next one on a connect/timeout failure.
echo ">> with two relays configured, killing the first must not cut B off"

# A is dead from section 5. Restart it (same data dir, so no fresh token) and
# add C as a second relay on its own port, so the list has two live hops.
ENROLL_TOKEN=""
e2e_start_node a "relay-a" "mock" "$TMP/work-a" "$TMP/repos-a" "$TMP/a" \
  AGENTGRID_RELAY_LISTEN="127.0.0.1:$RELAY_PORT" \
  AGENTGRID_RELAY_ALLOW="127.0.0.1/32"
A_PID=$NODE_PID

e2e_mint_token
e2e_start_node c "relay-c" "mock" "$TMP/work-c" "$TMP/repos-c" "$TMP/c" \
  AGENTGRID_RELAY_LISTEN="127.0.0.1:$RELAY2_PORT" \
  AGENTGRID_RELAY_ALLOW="127.0.0.1/32"
ENROLL_TOKEN=""

e2e_wait_nodes_online 3 || { echo "A/B/C are not all online"; tail -20 "$TMP/c.log"; exit 1; }
echo "   relays A and C are both up"

# Restart B against the list, A first — so it starts on A and has to rotate.
kill -9 "$B_PID" 2>/dev/null || true
wait "$B_PID" 2>/dev/null || true
e2e_forget_pid "$B_PID"
e2e_start_node b "relay-b" "mock" "$TMP/work-b" "$TMP/repos-b" "$TMP/b" \
  AGENTGRID_SERVERS="$RELAY,$RELAY2" \
  AGENTGRID_TRANSPORT=poll
B_PID=$NODE_PID
e2e_wait_nodes_online 3 || { echo "B did not come back on the relay list"; tail -30 "$TMP/b.log"; exit 1; }

BID=$(node_id_by_name "relay-b")
[ -n "$BID" ] || { echo "relay-b is not in /v1/nodes"; exit 1; }
TID=$(submit_pinned "write:relay-list-a.txt:hi" "$BID")
e2e_wait_terminal 120 "$TID" || { echo "task through the first relay never finished"; tail -30 "$TMP/b.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "task status=$STATUS"; tail -30 "$TMP/b.log"; exit 1; }
echo "   B is on the first relay and ran $TID"

# Kill the relay B is actually using. Nothing reconfigures B: the rotation on a
# failed connect is what has to carry it to C.
kill -9 "$A_PID" 2>/dev/null || true
wait "$A_PID" 2>/dev/null || true
e2e_forget_pid "$A_PID"

for _ in $(seq 1 80); do
  grep -q "rotating control-plane endpoint to $RELAY2" "$TMP/b.log" 2>/dev/null && break
  sleep 0.5
done
grep -q "rotating control-plane endpoint to $RELAY2" "$TMP/b.log" \
  || { echo "B never rotated onto the second relay"; tail -40 "$TMP/b.log"; exit 1; }
echo "   B rotated onto the second relay by itself"

e2e_wait_nodes_online 2 120 || { echo "B never came back online through the second relay"; tail -40 "$TMP/b.log"; exit 1; }

# The proof that it is really the second relay: a task pinned to B still
# succeeds, and C's own request counter moved.
BEFORE=$(metric_at "$RELAY2" agentgrid_edge_requests_total)
TID=$(submit_pinned "write:relay-list-b.txt:hi" "$BID")
e2e_wait_terminal 120 "$TID" || { echo "task through the second relay never finished"; tail -40 "$TMP/b.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "task status=$STATUS"; tail -40 "$TMP/b.log"; exit 1; }
AFTER=$(metric_at "$RELAY2" agentgrid_edge_requests_total)
[ "${AFTER:-0}" -gt "${BEFORE:-0}" ] \
  || { echo "second relay handled no new requests ($BEFORE -> $AFTER) — B did not move"; exit 1; }
echo "   task $TID pinned to relay-b: $STATUS, carried by the second relay ($BEFORE -> $AFTER requests)"

# --- 7. a relay that is alive but has lost its own upstream ----------------
# Section 5 was "the relay died" — a connect error, which rotates at once.
# This is the case nothing in the connect/timeout path can see: the hop answers,
# and answers `502 control-plane unreachable` because *its* route to the control
# plane is gone. A node used to sit on such an endpoint forever, backing off,
# while a working relay sat unused in its list.
echo ">> a relay answering 502 must be abandoned, but not on the first answer"

RELAY_BROKEN_PORT="${AGENTGRID_RELAY_BROKEN_PORT:-7834}"
RELAY_BROKEN="http://127.0.0.1:$RELAY_BROKEN_PORT"
# Nothing listens here: it stands in for "this host can no longer reach the
# control plane", which is the whole premise of a relay in the first place.
DEAD_CP="${AGENTGRID_DEAD_CP:-http://127.0.0.1:7839}"

# Node A comes back with the same credential (no token) and the same relay, but
# pointed at an unreachable control plane. The relay still starts — that is the
# point: the host is up, its uplink is not.
ENROLL_TOKEN=""
e2e_start_node a "relay-a" "mock" "$TMP/work-a" "$TMP/repos-a" "$TMP/a" \
  AGENTGRID_SERVER="$DEAD_CP" \
  AGENTGRID_RELAY_LISTEN="127.0.0.1:$RELAY_BROKEN_PORT" \
  AGENTGRID_RELAY_ALLOW="127.0.0.1/32"
A_PID=$NODE_PID

for _ in $(seq 1 60); do
  [ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "$RELAY_BROKEN/health/live" || true)" = "200" ] && break
  sleep 0.5
done
# The premise, asserted before blaming the node: the relay is up and answers
# 502, not a refused connection.
CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -X POST \
  -H 'content-type: application/json' -d '{}' "$RELAY_BROKEN/v1/node/poll" || true)
[ "$CODE" = "502" ] || { echo "expected 502 from the stranded relay, got $CODE"; tail -20 "$TMP/a.log"; exit 1; }
echo "   relay A is up (health/live 200) and answers $CODE for the control plane"

# B is told: stranded relay first, working relay second. Nothing reconfigures
# it afterwards — the gateway counter has to carry it across.
kill -9 "$B_PID" 2>/dev/null || true
wait "$B_PID" 2>/dev/null || true
e2e_forget_pid "$B_PID"
e2e_start_node b "relay-b" "mock" "$TMP/work-b" "$TMP/repos-b" "$TMP/b" \
  AGENTGRID_SERVERS="$RELAY_BROKEN,$RELAY2" \
  AGENTGRID_TRANSPORT=poll
B_PID=$NODE_PID

for _ in $(seq 1 120); do
  grep -q "gateway errors in a row" "$TMP/b.log" 2>/dev/null && break
  sleep 0.5
done
grep -q "gateway errors in a row from $RELAY_BROKEN; rotating control-plane endpoint to $RELAY2" \
  "$TMP/b.log" \
  || { echo "B never rotated off the 502 relay"; grep -iE "gateway|rotating|poll" "$TMP/b.log" | tail -20; exit 1; }

# The threshold, asserted rather than assumed: two failures are logged and the
# third is the one that rotates (it logs the rotation line, not "3/3"), so the
# rotation line's own count is the third data point. One failure before rotating
# would mean a single bad gateway moves a node — the behaviour this counter
# exists to avoid; more would mean the run never resets.
ROT_LINE=$(grep -n "gateway errors in a row" "$TMP/b.log" | head -1 | cut -d: -f1)
COUNTED=$(head -n $((ROT_LINE - 1)) "$TMP/b.log" | grep -cE "gateway failure [0-9]+/" || true)
[ "$COUNTED" = "2" ] \
  || { echo "expected 2 logged gateway failures before the third rotated (threshold 3), saw $COUNTED"; grep -iE "gateway" "$TMP/b.log" | head -10; exit 1; }
echo "   B left the 502 relay on the third gateway failure"

e2e_wait_nodes_online 2 120 || { echo "B never came back online through the working relay"; tail -40 "$TMP/b.log"; exit 1; }

BID=$(node_id_by_name "relay-b")
[ -n "$BID" ] || { echo "relay-b is not in /v1/nodes"; exit 1; }
BEFORE=$(metric_at "$RELAY2" agentgrid_edge_requests_total)
TID=$(submit_pinned "write:relay-gateway.txt:hi" "$BID")
e2e_wait_terminal 120 "$TID" || { echo "task after the gateway rotation never finished"; tail -40 "$TMP/b.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "task status=$STATUS"; tail -40 "$TMP/b.log"; exit 1; }
AFTER=$(metric_at "$RELAY2" agentgrid_edge_requests_total)
[ "${AFTER:-0}" -gt "${BEFORE:-0}" ] \
  || { echo "working relay handled no new requests ($BEFORE -> $AFTER) — B did not move"; exit 1; }
echo "   task $TID pinned to relay-b: $STATUS, through the working relay ($BEFORE -> $AFTER requests)"

echo "PASS: a node reached the control plane through a peer's relay, over poll and ws,"
echo "      survived the loss of the first of two relays, and left a relay that answered 502"
echo "      only after three gateway failures"
