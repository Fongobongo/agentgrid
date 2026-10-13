#!/usr/bin/env bash
# Slow-network failure injection (process-based, no Docker / no `tc`).
#
# Routes node→CP traffic through a throttling proxy (throttle-proxy.py) that
# sleeps DELAY_MS before every socket write, widening every round-trip. A
# chatty mock task (spam:200) must still complete with contiguous event
# sequences — no gaps, no duplicates, no spurious timeouts — under the
# inflated latency. This exercises the node's retry/idempotency path under
# slow-network conditions (failure-injection checklist item).
#
# Layout: CP on :7811 (real), proxy on :7820 (slow), node pointed at :7820.
# Operator traffic (setup/login/poll) goes direct; only the node is throttled.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE_REAL="http://127.0.0.1:7811"
BASE_PROXY="http://127.0.0.1:7820"
BASE="$BASE_REAL"
PORT=7811
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-stable-secret"
E2E_CONCURRENCY=2
# Only the node speaks through the proxy; CP/operator calls stay direct.
E2E_NODE_SERVER="$BASE_PROXY"

TMP="$(mktemp -d -t ag-e2e-slow-XXXXXX)"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos"

source "$ROOT/tests/e2e/lib-e2e.sh"

PROXY_PID=""
cleanup_slow() {
  e2e_cleanup
  [ -n "$PROXY_PID" ] && kill "$PROXY_PID" 2>/dev/null || true
  pkill -f "throttle-proxy.py" 2>/dev/null || true
}
trap cleanup_slow EXIT

start_proxy() {
  AG_PROXY_LISTEN="127.0.0.1:7820" \
  AG_PROXY_TARGET="127.0.0.1:7811" \
  AG_PROXY_DELAY_MS="${AG_E2E_SLOW_MS:-250}" \
  nohup python3 "$ROOT/tests/e2e/throttle-proxy.py" >"$TMP/proxy.log" 2>&1 &
  PROXY_PID=$!
}

echo ">> slow-network failure injection (delay=${AG_E2E_SLOW_MS:-250}ms/write)"

echo "  starting control plane on :7811"
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
# setup uses the real control-plane URL (not the slow proxy): /v1/auth/setup
# is a one-shot console op, run it without paying the throttle latency.
bootstrap_first_user "$TMP/cp.log" "$BASE_REAL" "$USER" "$PASS"
e2e_login
e2e_mint_token

echo "  starting throttle proxy :7820 -> :7811"
start_proxy
sleep 0.5

echo "  starting node pointed at proxy :7820"
e2e_start_node slow "e2e-node" "mock" "$TMP/work" "$TMP/repos" "$TMP/node"
e2e_wait_nodes_online 1 || exit 1

echo "  submitting chatty task spam:200 (200 events through slow link)"
TID=$(e2e_submit "spam:200" "mock" "*" 120)
echo "  task $TID; polling terminal (timeout 120s)"
if e2e_wait_terminal 120 "$TID"; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/slow.log"; exit 1
fi
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: expected succeeded, got $STATUS"; cat "$TMP/cp.log"; cat "$TMP/slow.log"; exit 1; }

echo "  checking event continuity (expect 200 spam lines, no loss/dups)"
# Grace: completion can overtake the last flush, and under the throttle with
# node concurrency=2 a straggler can lag behind (event N+1 visible while N is
# still queued) — so poll until the stream is whole, not just numerous.
# A true loss still fails below after the timeout, with full diagnostics.
#
# Invariant (deliberately GLOBAL, not stdout-only): the task sequence space
# is shared across event types — node-side events (e.g. child_peak_rss) and
# the adapter's own result event legitimately interleave with stdout lines
# (the CP even documents this outage class in store/attempts.rs). Asserting
# contiguity over stdout seqs alone false-positives on every such interleave.
# The true no-loss/no-dup invariant is: every sequence in [min..max] present
# exactly once across ALL types, plus every spam line present by content.
EV=""
for _ in $(seq 1 90); do
  EV=$(curl -fsS "$BASE_REAL/v1/tasks/$TID/events" -H "authorization: Bearer $jwt" 2>/dev/null || true)
  ok=$(printf '%s' "$EV" | python3 -c 'import sys,json
try:
  ev=json.load(sys.stdin)
  seqs=sorted(e["sequence"] for e in ev)
  texts=[(e.get("payload") or {}).get("text","") for e in ev if e.get("type")=="stdout"]
  whole=len(seqs)>=200 and seqs==list(range(seqs[0],seqs[-1]+1))
  spam=all(any(f"spam line {i}" in t for t in texts) for i in range(200))
  print(1 if (whole and spam) else 0)
except Exception:
  print(0)' 2>/dev/null)
  [ "${ok:-0}" = "1" ] && break
  sleep 1
done

printf '%s' "$EV" | python3 -c 'import sys,json
from collections import Counter
ev=json.load(sys.stdin)
seqs=sorted(e["sequence"] for e in ev)
types=Counter(e.get("type") for e in ev)
print(f"  event types: {dict(types)}")
print(f"  seq range: {seqs[0] if seqs else None}..{seqs[-1] if seqs else None} over {len(seqs)} events")
missing=[s for s in range(seqs[0],seqs[-1]+1) if s not in set(seqs)] if seqs else []
dups=[s for s,c in Counter(seqs).items() if c>1]
print(f"  missing seqs: {missing}")
print(f"  dup seqs: {dups}")
texts=[(e.get("payload") or {}).get("text","") for e in ev if e.get("type")=="stdout"]
absent=[i for i in range(200) if not any(f"spam line {i}" in t for t in texts)]
print(f"  missing spam lines: {absent[:10]}{\"...\" if len(absent)>10 else \"\"} (total {len(absent)})")
assert len(seqs)>=200, f"too few events: {len(seqs)}"
assert not missing, f"event loss: {missing}"
assert not dups, f"event duplicates: {dups}"
assert not absent, f"spam lines missing: {absent}"
print(f"  {len(seqs)} events, sequences {seqs[0]}..{seqs[-1]} exact, all 200 spam lines present")'

echo "  B OK: chatty task completed under slow network, no gaps/dups"
echo ">> slow-net OK"
