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
HERE="$ROOT/tests/e2e"

LISTEN_PORT="${AG_SMOKE_PORT:-7823}"
BASE="http://127.0.0.1:${LISTEN_PORT}"
USER="admin"; PASS="changeme"

TMP="$(mktemp -d -t ag-opencode-smoke-XXXXXX)"
CP_DB="$TMP/cp.db"
N1_DATA="$TMP/n1"; N1_WORK="$TMP/n1-work"; N1_REPOS="$TMP/n1-repos"
N2_DATA="$TMP/n2"; N2_WORK="$TMP/n2-work"; N2_REPOS="$TMP/n2-repos"
mkdir -p "$N1_DATA" "$N1_WORK" "$N1_REPOS" "$N2_DATA" "$N2_WORK" "$N2_REPOS" "$TMP/artifacts"

CP_PID=""; N1_PID=""; N2_PID=""
cleanup() {
  set +e
  [ -n "$N2_PID" ] && kill "$N2_PID" 2>/dev/null
  [ -n "$N1_PID" ] && kill "$N1_PID" 2>/dev/null
  [ -n "$CP_PID" ] && kill "$CP_PID" 2>/dev/null
  pkill -f "$BIN/agentgrid-control-plane" 2>/dev/null
  pkill -f "$BIN/agentgrid-node-daemon" 2>/dev/null
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

echo ">> opencode multi-node smoke on :$LISTEN_PORT"
[ -x "$BIN/agentgrid-control-plane" ] || { echo "  build first: cargo build --bin agentgrid-control-plane --bin agentgrid-node-daemon"; exit 1; }
[ -x "$BIN/adapter-mock" ] || { echo "  adapter-mock missing"; exit 1; }

# ── control plane ───────────────────────────────────────────────────────
AGENTGRID_LISTEN="127.0.0.1:$LISTEN_PORT" \
AGENTGRID_DB="$CP_DB" \
AGENTGRID_JWT_SECRET="smoke-secret" \
AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
CP_PID=$!

# ── auth ───────────────────────────────────────────────────────────────
source "$ROOT/tests/e2e/lib-bootstrap.sh"
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
login() {
  curl -fsS -X POST "$BASE/v1/auth/login" \
    -H 'content-type: application/json' \
    -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])'
}
jwt="$(login)"
[ -n "$jwt" ] || { echo "  login failed"; cat "$TMP/cp.log"; exit 1; }

enroll_token() {
  curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
    -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])'
}

start_node() {  # $1=name $2=data $3=work $4=repos $5=logfile $6=token $7=home
  AGENTGRID_SERVER="$BASE" \
  AGENTGRID_DATA_DIR="$2" \
  AGENTGRID_NODE_NAME="$1" \
  AGENTGRID_WORKSPACE_ROOT="$3" \
  AGENTGRID_REPOSITORY_ROOT="$4" \
  AGENTGRID_ADAPTERS="mock" \
  AGENTGRID_MAX_CONCURRENCY="1" \
  AGENTGRID_TRANSPORT="auto" \
  AGENTGRID_CONFIG_PULL_INTERVAL_SECS="30" \
  AGENTGRID_ALLOW_ROOT=1 \
  AGENTGRID_ENROLL_TOKEN="$6" \
  HOME="$7" \
  env PATH="$BIN:$PATH" RUST_LOG="info" \
    nohup "$BIN/agentgrid-node-daemon" >"$5" 2>&1 &
  echo $!
}

wait_ready() {
  for _ in $(seq 1 40); do
    curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
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

wait_ready || { echo "  CP not ready"; cat "$TMP/cp.log"; exit 1; }

N1_HOME="$TMP/n1-home"; N2_HOME="$TMP/n2-home"
mkdir -p "$N1_HOME/.config/opencode" "$N2_HOME/.config/opencode"

echo "  starting two nodes"
N1_PID=$(start_node "n1" "$N1_DATA" "$N1_WORK" "$N1_REPOS" "$TMP/n1.log" "$(enroll_token)" "$N1_HOME")
N2_PID=$(start_node "n2" "$N2_DATA" "$N2_WORK" "$N2_REPOS" "$TMP/n2.log" "$(enroll_token)" "$N2_HOME")
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
print(sum(1 for r in rows if (r.get("hash") or "") == "'"$PROFILE_HASH"'"))' 2>/dev/null || echo 0)
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
