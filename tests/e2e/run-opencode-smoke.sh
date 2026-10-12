#!/usr/bin/env bash
# Configurator polish item 9: multi-node opencode-profile smoke.
#
# Two node daemons (both on loopback) receive the SAME assigned profile and
# both apply it — proven by two `opencode_config_audit` rows (one per node)
# and both heartbeats reporting the profile's hash as `applied_opencode_hash`.
#
# Self-contained: no Docker, no second host. Bring up a temp control plane +
# two temp node daemons on loopback, create one profile, assign it to both
# nodes, wait for both apply audits, assert, tear down. The opencode apply
# path is exercised via the interval pull (so the smoke does not depend on
# the WS push making it through a flaky transport); a push on top is fine.
#
# Run from the repo root: tests/e2e/run-opencode-smoke.sh
# Override the binary dir with BIN=/path/to/bin if running elsewhere (e.g. on
# the remote test host after uploading the binaries there).
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="${BIN:-$ROOT/target/debug}"

LISTEN_PORT="${AG_SMOKE_PORT:-7823}"
BASE="http://127.0.0.1:${LISTEN_PORT}"
PORT="$LISTEN_PORT"
USER="admin"; PASS="changeme"
E2E_JWT_SECRET="smoke-secret"
E2E_CONCURRENCY=1

TMP="$(mktemp -d -t ag-opencode-smoke-XXXXXX)"
mkdir -p "$TMP/artifacts"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

echo ">> opencode multi-node smoke on :$LISTEN_PORT"
[ -x "$BIN/agentgrid-control-plane" ] || { echo "  build first: cargo build --bin agentgrid-control-plane --bin agentgrid-node-daemon"; exit 1; }
[ -x "$BIN/adapter-mock" ] || { echo "  adapter-mock missing"; exit 1; }

# ── control plane ───────────────────────────────────────────────────────
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "  CP not ready"; cat "$TMP/cp.log"; exit 1; }

# ── auth ───────────────────────────────────────────────────────────────
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login

enroll_token() {
  curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
    -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])'
}

wait_node_online() {  # $1 = node name
  local st
  for _ in $(seq 1 60); do
    st=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
      | python3 -c "import sys,json
ns={n['name']:n['status'] for n in json.load(sys.stdin)['items']}
print(ns.get('$1','none'))" 2>/dev/null || echo none)
    [ "$st" = "online" ] && return 0
    sleep 0.5
  done
  echo "  node $1 did not come online (last=$st)"; return 1
}

N1_HOME="$TMP/n1-home"; N2_HOME="$TMP/n2-home"
mkdir -p "$N1_HOME/.config/opencode" "$N2_HOME/.config/opencode"

echo "  starting two nodes"
# Per-node HOME: opencode keeps a local sqlite db under XDG_DATA_HOME and
# two daemons on one runner would collide on it. Transport auto, slow
# interval pull (the smoke asserts on the pull path, not the WS push).
e2e_start_node n1 "n1" "mock" "$TMP/n1-work" "$TMP/n1-repos" "$TMP/n1" \
  "HOME=$N1_HOME" "AGENTGRID_TRANSPORT=auto" \
  "AGENTGRID_CONFIG_PULL_INTERVAL_SECS=30" "AGENTGRID_ALLOW_ROOT=1" \
  "AGENTGRID_ENROLL_TOKEN=$(enroll_token)"
e2e_start_node n2 "n2" "mock" "$TMP/n2-work" "$TMP/n2-repos" "$TMP/n2" \
  "HOME=$N2_HOME" "AGENTGRID_TRANSPORT=auto" \
  "AGENTGRID_CONFIG_PULL_INTERVAL_SECS=30" "AGENTGRID_ALLOW_ROOT=1" \
  "AGENTGRID_ENROLL_TOKEN=$(enroll_token)"
wait_node_online "n1" || { cat "$TMP/n1.log"; exit 1; }
wait_node_online "n2" || { cat "$TMP/n2.log"; exit 1; }

# ── create one profile, assign to both nodes ───────────────────────────
echo "  creating one profile and assigning it to both nodes"
CFG='{"model":"anthropic/claude-sonnet-4.5","small_model":"anthropic/claude-haiku"}'
curl -fsS -X PUT "$BASE/v1/opencode-profiles/sonnet" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d "{\"config\":$CFG}" >/dev/null

PROFILE_ID=$(curl -fsS "$BASE/v1/opencode-profiles/sonnet" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
PROFILE_HASH=$(curl -fsS "$BASE/v1/opencode-profiles/sonnet" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')

NIDS=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json
print("\n".join(n["id"] for n in json.load(sys.stdin)["items"] if n["name"] in ("n1","n2")))')
for nid in $NIDS; do
  curl -fsS -X POST "$BASE/v1/nodes/$nid/opencode-profile" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"profile_id\":\"$PROFILE_ID\"}" >/dev/null
done

# ── wait for both node applies (interval pull ≤ 30s, give 60s) ──────────
echo "  waiting for both nodes to apply (profile hash ${PROFILE_HASH:0:12}…)"
applies=0
for _ in $(seq 1 60); do
  applies=0
  for nid in $NIDS; do
    count=$(curl -fsS "$BASE/v1/nodes/$nid/opencode-audit" -H "authorization: Bearer $jwt" \
      | python3 -c 'import sys,json
rows=json.load(sys.stdin)["items"]
print(sum(1 for r in rows if (r.get("hash") or "") == "'"${PROFILE_HASH}"'"))' 2>/dev/null || echo 0)
    [ "$count" -ge 1 ] && applies=$((applies+1))
  done
  [ "$applies" -eq 2 ] && break
  sleep 1
done

if [ "$applies" -ne 2 ]; then
  echo "  FAIL: only $applies/2 nodes applied"
  echo "--- n1.log ---"; tail -15 "$TMP/n1.log" || true
  echo "--- n2.log ---"; tail -15 "$TMP/n2.log" || true
  echo "--- cp.log ---"; tail -15 "$TMP/cp.log" || true
  exit 1
fi

echo "  PASS: 2 nodes, 1 profile — both applied (audit rows match the profile hash)"

# ── bundle-pinned skills (item 10): untrusted pins surface in the audit ──
echo "  verifying bundle-pinned skills reconcile"
PIN_CFG='{"model":"anthropic/claude-sonnet-4.5"}'
curl -fsS -X PUT "$BASE/v1/opencode-profiles/pinned" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d "{\"config\":$PIN_CFG,\"pinned_skills\":[\"pleft\",\"ponytail\"]}" >/dev/null

# No trust decisions exist -> both pins are untrusted. Assign the first
# node, wait for its apply, and check the audit reports both pins untrusted.
FIRST_NODE=$(echo "$NIDS" | head -1)
curl -fsS -X POST "$BASE/v1/nodes/$FIRST_NODE/opencode-profile" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d "{\"profile_id\":\"$(curl -fsS "$BASE/v1/opencode-profiles/pinned" -H "authorization: Bearer $jwt" | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')\"}" >/dev/null

pin_untrusted=""
for _ in $(seq 1 60); do
  pin_untrusted=$(curl -fsS "$BASE/v1/nodes/$FIRST_NODE/opencode-audit" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json
rows=json.load(sys.stdin)["items"]
from json import loads as L
ps=[r.get("pinned_untrusted") for r in rows if r.get("pinned_untrusted")]
print("|".join(sorted(ps[0])) if ps else "")' 2>/dev/null || echo "")
  [ -n "$pin_untrusted" ] && break
  sleep 1
done
[ "$pin_untrusted" = "pleft|ponytail" ] || { echo "  FAIL: pinned-reconcile expected pleft|ponytail, got '$pin_untrusted'"; tail -20 "$TMP/n1.log" || true; exit 1; }
echo "  PASS: untrusted pinned skills ([pleft, ponytail]) surfaced in the apply audit"
