#!/usr/bin/env bash
# E2E (process-based, no Docker): HA partition/heal chaos.
#
# Simulates a network partition by SIGSTOPping the leader (it stays alive but
# unreachable) and asserts a new leader elects, the edge forwards to it, and a
# task completes. Then SIGCONT the old leader (partition heals) and asserts it
# rejoins as a standby with exactly one leader — no dual writers.
#
# Uses an agentgrid-cp-edge in front so client traffic follows the leader.
#
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
CP_BIN="${AGENTGRID_CP_BIN:-$ROOT/target/debug/agentgrid-control-plane}"
EDGE_BIN="${AGENTGRID_EDGE_BIN:-$ROOT/target/debug/agentgrid-cp-edge}"
NODE_BIN="${AGENTGRID_NODE_BIN:-$ROOT/target/debug/agentgrid-node-daemon}"
ADAPTER_BIN="$ROOT/target/debug"

TOKEN="e2e-ha-part-token-0123456789abcdef"
JWT_SECRET="e2e-ha-part-secret-0123456789abcdef0123456789"
CLUSTER="e2e-part"
USER="admin"
PASS="changeme"

declare -A PORT=([cp-a]=17921 [cp-b]=17922 [cp-c]=17923 [edge]=17924)
declare -A PID=()
STOPPED=""
TMP="$(mktemp -d -t ag-e2e-ha-part-XXXXXX)"
NODE_PID=""

cleanup() {
  set +e
  [ -n "$STOPPED" ] && kill -CONT "$STOPPED" 2>/dev/null
  [ -n "$NODE_PID" ] && kill -9 "$NODE_PID" 2>/dev/null
  for pid in "${PID[@]:-}"; do kill -9 "$pid" 2>/dev/null; done
  wait 2>/dev/null
  if [ "${AG_E2E_KEEP:-}" = "1" ]; then echo "logs kept in $TMP"; else rm -rf "$TMP"; fi
}
trap cleanup EXIT

for b in "$CP_BIN" "$EDGE_BIN" "$NODE_BIN" "$ADAPTER_BIN/adapter-mock"; do
  [ -x "$b" ] || { echo "missing $b (cargo build first)" >&2; exit 1; }
done

start_node() {  # $1 = id
  local id="$1"
  local dir="$TMP/$id"
  mkdir -p "$dir"
  local peers
  case "$id" in
    cp-a) peers="cp-b@http://127.0.0.1:${PORT[cp-b]},cp-c@http://127.0.0.1:${PORT[cp-c]}";;
    cp-b) peers="cp-a@http://127.0.0.1:${PORT[cp-a]},cp-c@http://127.0.0.1:${PORT[cp-c]}";;
    cp-c) peers="cp-a@http://127.0.0.1:${PORT[cp-a]},cp-b@http://127.0.0.1:${PORT[cp-b]}";;
  esac
  env AGENTGRID_LISTEN="127.0.0.1:${PORT[$id]}" \
      AGENTGRID_DB="$dir/control-plane.db" \
      AGENTGRID_JWT_SECRET="$JWT_SECRET" \
      AGENTGRID_HA_MODE=ha \
      AGENTGRID_HA_ROLE=data \
      AGENTGRID_HA_NODE_ID="$id" \
      AGENTGRID_HA_CLUSTER_ID="$CLUSTER" \
      AGENTGRID_HA_CLUSTER_TOKEN="$TOKEN" \
      AGENTGRID_HA_ADVERTISE_URL="http://127.0.0.1:${PORT[$id]}" \
      AGENTGRID_HA_PEERS="$peers" \
      AGENTGRID_HA_WITNESSES="" \
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
      AGENTGRID_EDGE_UPSTREAMS="http://127.0.0.1:${PORT[cp-a]},http://127.0.0.1:${PORT[cp-b]},http://127.0.0.1:${PORT[cp-c]}" \
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
  for id in cp-a cp-b cp-c; do
    local s
    s=$(ha_status "${PORT[$id]}" 2>/dev/null || true)
    [ -n "$s" ] || continue
    [ "$(field "$s" role)" = "leader" ] && n=$((n + 1))
  done
  echo "$n"
}
leader_id() {
  for id in cp-a cp-b cp-c; do
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

echo ">> HA partition/heal: starting 3 data nodes + edge"
start_node cp-a
start_node cp-b
start_node cp-c
start_edge
leader=$(wait_leader 30) || { echo "FAIL: no single leader" >&2; exit 1; }
echo "  initial leader=$leader"

EDGE_BASE="http://127.0.0.1:${PORT[edge]}"

echo ">> bootstrap admin (via edge)"
setup_token=$(awk '/agentgrid setup token/{getline; print; exit}' "$TMP/$leader.log")
[ -n "$setup_token" ] || { echo "FAIL: setup token not found" >&2; exit 1; }
curl -fsS -H 'content-type: application/json' -o /dev/null \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\",\"setup_token\":\"$setup_token\"}" \
  "$EDGE_BASE/v1/auth/setup"
jwt=$(curl -fsS -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  "$EDGE_BASE/v1/auth/login" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

echo ">> partition the leader ($leader) with SIGSTOP"
kill -STOP "${PID[$leader]}"
STOPPED="${PID[$leader]}"
sleep 2  # let lease expire + election

echo ">> wait for a new leader during partition"
new_leader=$(wait_leader 60) || { echo "FAIL: no leader during partition" >&2; exit 1; }
echo "  new leader during partition=$new_leader"
[ "$new_leader" != "$leader" ] || { echo "FAIL: old leader still leading during partition" >&2; exit 1; }

echo ">> task completes via edge during partition"
task=$(curl -fsS -X POST -H "authorization: Bearer $jwt" \
  -H 'content-type: application/json' \
  -d '{"goal":"partition smoke","adapter":"mock","repository":null}' \
  "$EDGE_BASE/v1/tasks" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
[ -n "$task" ] || { echo "FAIL: task create during partition" >&2; exit 1; }
ok=0
for _ in $(seq 1 120); do
  st=$(curl -fsS -H "authorization: Bearer $jwt" "$EDGE_BASE/v1/tasks/$task" 2>/dev/null \
    | python3 -c 'import sys,json
try: print(json.load(sys.stdin).get("status","?"))
except Exception: print("?")' 2>/dev/null || echo "?")
  [ "$st" = "succeeded" ] && ok=1 && break
  [ "$st" = "failed" ] && { echo "FAIL: task failed during partition" >&2; exit 1; }
  sleep 0.5
done
[ "$ok" = "1" ] || { echo "FAIL: task did not succeed during partition" >&2; exit 1; }

echo ">> heal partition: SIGCONT old leader ($leader)"
kill -CONT "${PID[$leader]}" 2>/dev/null || true
STOPPED=""

echo ">> wait for exactly one leader after heal (old leader steps down)"
for _ in $(seq 1 150); do
  if [ "$(count_leaders)" = 1 ]; then
    l=$(leader_id)
    [ "$l" = "$new_leader" ] && break
  fi
  sleep 0.2
done
[ "$(leader_id)" = "$new_leader" ] || { echo "FAIL: leader after heal=$new_leader expected" >&2; exit 1; }
[ "$(count_leaders)" = "1" ] || { echo "FAIL: $(count_leaders) leaders after heal" >&2; exit 1; }
echo "PASS: partition/heal (new leader during partition, old leader steps down, no dual writers)"
