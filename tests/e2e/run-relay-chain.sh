#!/usr/bin/env bash
# E2E (process-based, no Docker, no LLM): two relays in a chain.
#
# ADR 0026 shipped a relay that carries a peer's traffic to *its own* control
# plane and said "one hop only" — the upstream was a single URL and the only
# loop check was a static self-reference, so chaining two relays was neither
# tested nor safe. This proves the chain works and that it cannot loop:
#
#     node C  ──▶  relay B  ──▶  relay A  ──▶  control plane
#     (knows only B)          (knows only A)
#
# Neither B nor C is configured with the control-plane address, so if C comes
# online and runs a task, its traffic went through both relays — there is no
# other address it could have dialled.
#
# What is asserted:
#   1. three nodes online with C knowing only B and B knowing only A;
#   2. a task pinned to C succeeds (a real payload, two hops);
#   3. both relays report carrying requests, and neither reports a loop;
#   4. the hop counter is incremented per relay, not per client: a request
#      entering B with 1 hop arrives at A as 2 and is forwarded, while one
#      entering with 2 arrives at A as 3 and is refused with 508 — the same
#      header, two different outcomes, decided one hop apart.
#
# Honest limit: every process is on 127.0.0.1, so "cannot reach the control
# plane" is expressed by configuration rather than a firewall — same caveat as
# run-relay.sh. The relay ports are distinct from that script's so the two can
# run back to back in one CI job.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="${AGENTGRID_BIN:-$ROOT/target/debug}"

PORT="${AGENTGRID_PORT:-7840}"
BASE="http://127.0.0.1:$PORT"
RELAY_A_PORT="${AGENTGRID_RELAY_A_PORT:-7842}"
RELAY_A="http://127.0.0.1:$RELAY_A_PORT"
RELAY_B_PORT="${AGENTGRID_RELAY_B_PORT:-7843}"
RELAY_B="http://127.0.0.1:$RELAY_B_PORT"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-relay-chain-secret"
E2E_CONCURRENCY=2

# A dev box may export HTTP_PROXY (this one does) and reqwest honours it, which
# would put a proxy between these hops and make the chain untestable. Pin
# direct localhost traffic for every child and for curl.
export NO_PROXY="127.0.0.1,localhost"
export no_proxy="127.0.0.1,localhost"

TMP="$(mktemp -d -t ag-e2e-relay-chain-XXXXXX)"
for d in a b c work-a work-b work-c repos-a repos-b repos-c; do mkdir -p "$TMP/$d"; done

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

json_str() { python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1"; }

node_id_by_name() {  # $1 = node name
  curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json
d=json.load(sys.stdin); ns=d.get("items",d) if isinstance(d,dict) else d
print(next((n["id"] for n in ns if n.get("name")==sys.argv[1]), ""))' "$1"
}

submit_pinned() {  # $1 = prompt, $2 = node id
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$(json_str "$1"),\"repository\":\"*\",\"adapter\":\"mock\",\"timeout_secs\":120,\"requested_node_id\":\"$2\"}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}

metric_at() {  # $1 = relay base, $2 = metric name
  curl -fsS "$1/metrics" 2>/dev/null \
    | awk -v m="$2" '$1==m {print $2; found=1} END{if(!found) print 0}' || echo 0
}

# A forwarded request through relay B, carrying a given hop count. Prints the
# HTTP status; the body is left for the caller to inspect if it cares.
status_through_b() {  # $1 = hop count to enter with
  curl -s -o "$TMP/last-body" -w '%{http_code}' --max-time 10 \
    -H "x-agentgrid-relay-hops: $1" "$RELAY_B/v1/ha/status" || true
}

echo ">> relay chain E2E: C -> relay B -> relay A -> control plane"

e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token

# --- 1. the first hop: node A, which can reach the control plane ----------
echo ">> node A relays for others (it is the only node that knows the CP)"
e2e_start_node a "chain-a" "mock" "$TMP/work-a" "$TMP/repos-a" "$TMP/a" \
  AGENTGRID_RELAY_LISTEN="127.0.0.1:$RELAY_A_PORT" \
  AGENTGRID_RELAY_ALLOW="127.0.0.1/32"
e2e_wait_nodes_online 1 || { echo "node A never came online"; tail -20 "$TMP/a.log"; exit 1; }
curl -fsS --max-time 5 "$RELAY_A/health/ready" >/dev/null \
  || { echo "relay A is not ready"; tail -20 "$TMP/a.log"; exit 1; }

# --- 2. the second hop: node B knows only relay A -------------------------
echo ">> node B knows only relay A, and relays for others itself"
e2e_mint_token
e2e_start_node b "chain-b" "mock" "$TMP/work-b" "$TMP/repos-b" "$TMP/b" \
  AGENTGRID_SERVER="$RELAY_A" \
  AGENTGRID_TRANSPORT=poll \
  AGENTGRID_RELAY_LISTEN="127.0.0.1:$RELAY_B_PORT" \
  AGENTGRID_RELAY_ALLOW="127.0.0.1/32"
e2e_wait_nodes_online 2 || { echo "B never registered through relay A"; tail -30 "$TMP/b.log"; exit 1; }
curl -fsS --max-time 5 "$RELAY_B/health/ready" >/dev/null \
  || { echo "relay B is not ready"; tail -20 "$TMP/b.log"; exit 1; }
echo "   relay B is up (it reaches the CP only through relay A)"

# --- 3. the far end: node C knows only relay B ---------------------------
echo ">> node C knows only relay B — two hops from the control plane"
e2e_mint_token
e2e_start_node c "chain-c" "mock" "$TMP/work-c" "$TMP/repos-c" "$TMP/c" \
  AGENTGRID_SERVER="$RELAY_B" \
  AGENTGRID_TRANSPORT=poll
e2e_wait_nodes_online 3 || { echo "C never registered through both relays"; tail -30 "$TMP/c.log"; exit 1; }

CID=$(node_id_by_name "chain-c")
[ -n "$CID" ] || { echo "chain-c is not in /v1/nodes"; exit 1; }
echo "   chain-c registered through two relays as $CID"

# --- 4. a real task travels the whole chain ------------------------------
echo ">> a task pinned to C, which no relay can shortcut"
TID=$(submit_pinned "write:chained.txt:hi" "$CID")
e2e_wait_terminal 120 "$TID" || { echo "pinned task never finished"; tail -30 "$TMP/c.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "task status=$STATUS"; tail -30 "$TMP/c.log"; exit 1; }
echo "   task $TID pinned to chain-c: $STATUS"

# --- 5. both relays carried traffic, and neither saw a loop --------------
A_REQS=$(metric_at "$RELAY_A" agentgrid_edge_requests_total)
B_REQS=$(metric_at "$RELAY_B" agentgrid_edge_requests_total)
[ "${A_REQS:-0}" -gt 0 ] || { echo "relay A carried 0 requests — C's traffic never arrived"; exit 1; }
[ "${B_REQS:-0}" -gt 0 ] || { echo "relay B carried 0 requests — C bypassed it"; exit 1; }
A_LOOPS=$(metric_at "$RELAY_A" agentgrid_edge_loop_detected_total)
B_LOOPS=$(metric_at "$RELAY_B" agentgrid_edge_loop_detected_total)
[ "${A_LOOPS:-1}" = "0" ] || { echo "relay A reported a loop during a working chain: $A_LOOPS"; exit 1; }
[ "${B_LOOPS:-1}" = "0" ] || { echo "relay B reported a loop during a working chain: $B_LOOPS"; exit 1; }
echo "   relay A carried $A_REQS, relay B carried $B_REQS, no loops on either"

# --- 6. the counter is per relay, not per client -------------------------
# Both requests below enter at relay B; the only difference is the number they
# arrive with, and the outcome flips one hop later at relay A. If the header
# were passed through unchanged (or reset at each hop) these two would agree.
echo ">> the same request, one hop apart, gets opposite answers"
CODE=$(status_through_b 1)
[ "$CODE" != "508" ] \
  || { echo "entering B with 1 hop was refused at A (status $CODE) — the counter is off by one"; cat "$TMP/last-body"; exit 1; }
echo "   entering with 1 hop -> A sees 2 -> forwarded (status $CODE)"

CODE=$(status_through_b 2)
[ "$CODE" = "508" ] \
  || { echo "entering B with 2 hops should be refused at A, got $CODE"; cat "$TMP/last-body"; exit 1; }
grep -qi "relay loop" "$TMP/last-body" \
  || { echo "508 did not explain itself:"; cat "$TMP/last-body"; exit 1; }
A_LOOPS_AFTER=$(metric_at "$RELAY_A" agentgrid_edge_loop_detected_total)
[ "${A_LOOPS_AFTER:-0}" -ge 1 ] \
  || { echo "relay A answered 508 but its loop counter stayed at ${A_LOOPS_AFTER:-0}"; exit 1; }
echo "   entering with 2 hops -> A sees 3 -> 508 relay loop (A's counter: $A_LOOPS_AFTER)"

echo "OK: two relays chained, a task ran through both, and a loop is cut with 508"
