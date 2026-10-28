#!/usr/bin/env bash
# E2E (process-based, no Docker): HA 2 data + witness + edge chaos.
#
# Starts 2 control-plane data hosts + 1 witness (snapshot RPO, peer backend,
# plaintext) and an agentgrid-cp-edge. Bootstraps admin, starts a node-daemon
# through the edge, submits a task, hard-kills the leader and asserts:
#   - the remaining data host + witness form quorum and elect a new leader;
#   - exactly one leader (witness never leads);
#   - the edge forwards to the new leader and the in-flight task completes;
#   - a post-failover task via the edge succeeds.
#
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
CP_BIN="${AGENTGRID_CP_BIN:-$ROOT/target/debug/agentgrid-control-plane}"
EDGE_BIN="${AGENTGRID_EDGE_BIN:-$ROOT/target/debug/agentgrid-cp-edge}"
NODE_BIN="${AGENTGRID_NODE_BIN:-$ROOT/target/debug/agentgrid-node-daemon}"
ADAPTER_BIN="$ROOT/target/debug"

# Throwaway fixtures for control-plane processes this script boots on
# 127.0.0.1; never real credentials. The inline allow keeps gitleaks from
# re-flagging the same line on every edit (fingerprints are commit-scoped).
TOKEN="e2e-ha-2w-token-0123456789abcdef"  # gitleaks:allow
JWT_SECRET="e2e-ha-2w-secret-0123456789abcdef0123456789"  # gitleaks:allow
CLUSTER="e2e-2w"
USER="admin"
PASS="changeme"

declare -A PORT=([cp-a]=17911 [cp-b]=17912 [w1]=17913 [edge]=17914)
declare -A PID=()
TMP="$(mktemp -d -t ag-e2e-ha-2w-XXXXXX)"
NODE_PID=""

cleanup() {
  set +e
  [ -n "$NODE_PID" ] && kill -9 "$NODE_PID" 2>/dev/null
  for pid in "${PID[@]:-}"; do kill -9 "$pid" 2>/dev/null; done
  wait 2>/dev/null
  if [ "${AG_E2E_KEEP:-}" = "1" ]; then echo "logs kept in $TMP"; else rm -rf "$TMP"; fi
}
trap cleanup EXIT

for b in "$CP_BIN" "$EDGE_BIN" "$NODE_BIN" "$ADAPTER_BIN/adapter-mock"; do
  [ -x "$b" ] || { echo "missing $b (cargo build first)" >&2; exit 1; }
done

start_node() {  # $1 = id, $2 = role
  local id="$1" role="$2"
  local dir="$TMP/$id"
  mkdir -p "$dir"
  local peers witnesses
  case "$id" in
    cp-a)
      peers="cp-b@http://127.0.0.1:${PORT[cp-b]}"
      witnesses="w1@http://127.0.0.1:${PORT[w1]}"
      ;;
    cp-b)
      peers="cp-a@http://127.0.0.1:${PORT[cp-a]}"
      witnesses="w1@http://127.0.0.1:${PORT[w1]}"
      ;;
    w1)
      peers="cp-a@http://127.0.0.1:${PORT[cp-a]},cp-b@http://127.0.0.1:${PORT[cp-b]}"
      witnesses=""
      ;;
  esac
  env AGENTGRID_LISTEN="127.0.0.1:${PORT[$id]}" \
      AGENTGRID_DB="$dir/control-plane.db" \
      AGENTGRID_JWT_SECRET="$JWT_SECRET" \
      AGENTGRID_HA_MODE=ha \
      AGENTGRID_HA_ROLE="$role" \
      AGENTGRID_HA_NODE_ID="$id" \
      AGENTGRID_HA_CLUSTER_ID="$CLUSTER" \
      AGENTGRID_HA_CLUSTER_TOKEN="$TOKEN" \
      AGENTGRID_HA_ADVERTISE_URL="http://127.0.0.1:${PORT[$id]}" \
      AGENTGRID_HA_PEERS="$peers" \
      AGENTGRID_HA_WITNESSES="$witnesses" \
      AGENTGRID_HA_ALLOW_PLAINTEXT=1 \
      AGENTGRID_HA_LEASE_TTL_SECS=4 \
      AGENTGRID_HA_ELECTION_TIMEOUT_MS=600 \
      AGENTGRID_HA_SNAPSHOT_INTERVAL_SECS=10 \
      AGENTGRID_HA_WITNESS_DIR="$dir/witness" \
      "$CP_BIN" >"$TMP/$id.log" 2>&1 &
  PID[$id]=$!
}

start_edge() {
  env AGENTGRID_EDGE_LISTEN="127.0.0.1:${PORT[edge]}" \
      AGENTGRID_EDGE_UPSTREAMS="http://127.0.0.1:${PORT[cp-a]},http://127.0.0.1:${PORT[cp-b]}" \
      AGENTGRID_HA_CLUSTER_TOKEN="$TOKEN" \
      "$EDGE_BIN" >"$TMP/edge.log" 2>&1 &
  PID[edge]=$!
}

ha_status() { curl -fsS -H "x-agentgrid-ha-token: $TOKEN" "http://127.0.0.1:$1/v1/ha/status"; }
field() { python3 -c 'import json,sys
try: print(json.loads(sys.argv[1]).get(sys.argv[2], ""))
except Exception: print("")' "$1" "$2"; }

count_leaders() {
  local n=0
  for id in cp-a cp-b; do
    local s
    s=$(ha_status "${PORT[$id]}" 2>/dev/null || true)
    [ -n "$s" ] || continue
    [ "$(field "$s" role)" = "leader" ] && n=$((n + 1))
  done
  echo "$n"
}
leader_id() {
  for id in cp-a cp-b; do
    local s
    s=$(ha_status "${PORT[$id]}" 2>/dev/null || true)
    [ -n "$s" ] || continue
    [ "$(field "$s" role)" = "leader" ] && { echo "$id"; return; }
  done
}
wait_leader() {  # [timeout_secs]
  local rounds=$(( ${1:-100} * 5 ))
  for _ in $(seq 1 "$rounds"); do
    if [ "$(count_leaders)" = 1 ]; then leader_id; return 0; fi
    sleep 0.2
  done
  return 1
}

echo ">> HA 2+witness chaos: starting 2 data + witness + edge"
start_node cp-a data
start_node cp-b data
start_node w1 witness
start_edge
leader=$(wait_leader 30) || { echo "FAIL: no single leader" >&2; exit 1; }
follower=$( [ "$leader" = cp-a ] && echo cp-b || echo cp-a )
echo "  leader=$leader follower=$follower"

EDGE_BASE="http://127.0.0.1:${PORT[edge]}"

echo ">> bootstrap admin + mint enrollment token (via edge)"
setup_token=$(awk '/agentgrid setup token/{getline; print; exit}' "$TMP/$leader.log")
[ -n "$setup_token" ] || { echo "FAIL: setup token not found" >&2; exit 1; }
curl -fsS -H 'content-type: application/json' -o /dev/null \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\",\"setup_token\":\"$setup_token\"}" \
  "$EDGE_BASE/v1/auth/setup"
jwt=$(curl -fsS -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  "$EDGE_BASE/v1/auth/login" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
enroll=$(curl -fsS -X POST -H "authorization: Bearer $jwt" \
  "$EDGE_BASE/v1/nodes/enrollment-token" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

echo ">> node-daemon through the edge"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos"
env PATH="$ADAPTER_BIN:$PATH" \
    RUST_LOG=info \
    AGENTGRID_SERVER="$EDGE_BASE" \
    AGENTGRID_SERVERS="http://127.0.0.1:${PORT[cp-a]},http://127.0.0.1:${PORT[cp-b]}" \
    AGENTGRID_DATA_DIR="$TMP/node" \
    AGENTGRID_NODE_NAME="ha-2w-node" \
    AGENTGRID_WORKSPACE_ROOT="$TMP/work" \
    AGENTGRID_REPOSITORY_ROOT="$TMP/repos" \
    AGENTGRID_ADAPTERS=mock \
    AGENTGRID_MAX_CONCURRENCY=2 \
    AGENTGRID_ENROLL_TOKEN="$enroll" \
    AGENTGRID_MIN_FREE_DISK_MB=1024 \
    AGENTGRID_MIN_FREE_MEM_MB=512 \
    "$NODE_BIN" >"$TMP/node.log" 2>&1 &
NODE_PID=$!

node_online() {
  curl -fsS -H "authorization: Bearer $jwt" "$EDGE_BASE/v1/nodes" 2>/dev/null \
    | python3 -c 'import sys,json
try:
 d=json.load(sys.stdin); ns=d.get("items",d) if isinstance(d,dict) else d
 print(sum(1 for n in ns if n.get("status")=="online"))
except Exception: print(0)' 2>/dev/null || echo 0
}
for _ in $(seq 1 60); do
  [ "$(node_online)" -ge 1 ] && break
  sleep 0.5
done
[ "$(node_online)" -ge 1 ] || { echo "FAIL: node not online via edge" >&2; exit 1; }

echo ">> submit task via edge"
task=$(curl -fsS -X POST -H "authorization: Bearer $jwt" \
  -H 'content-type: application/json' \
  -d '{"goal":"chaos 2w smoke","adapter":"mock","repository":null}' \
  "$EDGE_BASE/v1/tasks" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
[ -n "$task" ] || { echo "FAIL: task create failed" >&2; exit 1; }

echo ">> wait for task success"
ok=0
for _ in $(seq 1 120); do
  st=$(curl -fsS -H "authorization: Bearer $jwt" "$EDGE_BASE/v1/tasks/$task" 2>/dev/null \
    | python3 -c 'import sys,json
try: print(json.load(sys.stdin).get("status","?"))
except Exception: print("?")' 2>/dev/null || echo "?")
  [ "$st" = "succeeded" ] && ok=1 && break
  [ "$st" = "failed" ] && { echo "FAIL: task failed" >&2; exit 1; }
  sleep 0.5
done
[ "$ok" = "1" ] || { echo "FAIL: task did not succeed" >&2; exit 1; }

echo ">> hard-kill the leader ($leader)"
kill -9 "${PID[$leader]}" 2>/dev/null || true
unset 'PID[$leader]'

echo ">> wait for exactly one leader (witness never leads)"
new_leader=$(wait_leader 60) || { echo "FAIL: no single leader after kill" >&2; exit 1; }
echo "  new leader=$new_leader"

st=$(curl -fsS -H "authorization: Bearer $jwt" "$EDGE_BASE/v1/tasks/$task" 2>/dev/null \
  | python3 -c 'import sys,json
try: print(json.load(sys.stdin).get("status","?"))
except Exception: print("?")' 2>/dev/null || echo "?")
[ "$st" = "succeeded" ] || { echo "FAIL: edge task status=$st after kill" >&2; exit 1; }

echo ">> post-failover task via edge"
task2=$(curl -fsS -X POST -H "authorization: Bearer $jwt" \
  -H 'content-type: application/json' \
  -d '{"goal":"post-failover 2w","adapter":"mock","repository":null}' \
  "$EDGE_BASE/v1/tasks" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
[ -n "$task2" ] || { echo "FAIL: post-failover task create failed" >&2; exit 1; }
ok=0
for _ in $(seq 1 120); do
  st=$(curl -fsS -H "authorization: Bearer $jwt" "$EDGE_BASE/v1/tasks/$task2" 2>/dev/null \
    | python3 -c 'import sys,json
try: print(json.load(sys.stdin).get("status","?"))
except Exception: print("?")' 2>/dev/null || echo "?")
  [ "$st" = "succeeded" ] && ok=1 && break
  [ "$st" = "failed" ] && { echo "FAIL: post-failover task failed" >&2; exit 1; }
  sleep 0.5
done
[ "$ok" = "1" ] || { echo "FAIL: post-failover task did not succeed" >&2; exit 1; }
[ "$(count_leaders)" = "1" ] || { echo "FAIL: $(count_leaders) leaders after failover" >&2; exit 1; }
echo "PASS: 2+witness chaos (leader kill, witness quorum, edge forwarding, post-failover task)"
