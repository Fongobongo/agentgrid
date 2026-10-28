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

# Throwaway fixtures for control-plane processes this script boots on
# 127.0.0.1; never real credentials. The inline allow keeps gitleaks from
# re-flagging the same line on every edit (fingerprints are commit-scoped).
TOKEN="e2e-ha-part-token-0123456789abcdef"  # gitleaks:allow
JWT_SECRET="e2e-ha-part-secret-0123456789abcdef0123456789"  # gitleaks:allow
CLUSTER="e2e-part"
USER="admin"
PASS="changeme"

declare -A PORT=([cp-a]=17921 [cp-b]=17922 [cp-c]=17923 [edge]=17924)
DATA_IDS=(cp-a cp-b cp-c)
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
      AGENTGRID_HA_SNAPSHOT_INTERVAL_SECS=2 \
      AGENTGRID_HA_WITNESS_DIR="$dir/witness" \
      "$CP_BIN" >"$TMP/$id.log" 2>&1 &
  PID[$id]=$!
}

start_edge() {
  env AGENTGRID_EDGE_LISTEN="127.0.0.1:${PORT[edge]}" \
      AGENTGRID_EDGE_UPSTREAMS="http://127.0.0.1:${PORT[cp-a]},http://127.0.0.1:${PORT[cp-b]},http://127.0.0.1:${PORT[cp-c]}" \
      AGENTGRID_HA_CLUSTER_TOKEN="$TOKEN" \
      AGENTGRID_EDGE_UPSTREAM_TIMEOUT_SECS=2 \
      "$EDGE_BIN" >"$TMP/edge.log" 2>&1 &
  PID[edge]=$!
}

ha_status() { curl -fsS --max-time 2 -H "x-agentgrid-ha-token: $TOKEN" "http://127.0.0.1:$1/v1/ha/status"; }

# Create a task through the edge and print its id. The edge documents
# `503 + Retry-After` for a leaderless moment, so a client is expected to
# retry: a failover leaves a short window where no host has claimed
# leadership yet, and a leader that just won may still be arming its write
# gate. Anything other than 200/201/503 is a real failure.
create_task() {  # $1 = prompt
  local prompt="$1" code out="$TMP/task-create.json"
  for _ in $(seq 1 20); do
    code=$(curl -sS --max-time 15 --connect-timeout 5 -o "$out" -w '%{http_code}' -X POST \
      -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
      -d "{\"prompt\":\"$prompt\",\"repository\":\"*\",\"adapter\":\"mock\",\"timeout_secs\":60}" \
      "$EDGE_BASE/v1/tasks" 2>/dev/null || true)
    case "${code:-000}" in
      200|201) python3 -c 'import sys,json;print(json.load(open(sys.argv[1])).get("id",""))' "$out"; return 0 ;;
      503|000) echo "  task create got ${code:-000}; retrying (leaderless window)" >&2; sleep 1 ;;
      *) echo "  task create failed: http=${code:-000}" >&2; return 1 ;;
    esac
  done
  return 1
}
field() { python3 -c 'import json,sys
try: print(json.loads(sys.argv[1]).get(sys.argv[2], ""))
except Exception: print("")' "$1" "$2"; }

# Wait until every non-leader data host has applied at least one replicated
# sequence. Killing the leader before that hands the cluster to a replica with
# an empty database: `require_user_auth` then answers a bare 503 (no body, no
# role header) on every `/v1/` route, because the bootstrap guard closes the
# API when `user_count() == 0`. That looks exactly like an edge failure and is
# not one.
wait_replicated() {  # $1 = leader id, $2 = timeout secs
  local leader="$1" rounds=$(( ${2:-30} * 5 )) id seq all
  for _ in $(seq 1 "$rounds"); do
    all=1
    for id in "${DATA_IDS[@]}"; do
      [ "$id" = "$leader" ] && continue
      seq=$(field "$(ha_status "${PORT[$id]}" 2>/dev/null || true)" applied_seq)
      case "$seq" in ''|*[!0-9]*) all=0 ;; *) [ "$seq" -ge 1 ] || all=0 ;; esac
    done
    [ "$all" = 1 ] && return 0
    sleep 0.2
  done
  return 1
}

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
curl -fsS --max-time 15 --connect-timeout 5 -H 'content-type: application/json' -o /dev/null \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\",\"setup_token\":\"$setup_token\"}" \
  "$EDGE_BASE/v1/auth/setup"
jwt=$(curl -fsS --max-time 15 --connect-timeout 5 -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  "$EDGE_BASE/v1/auth/login" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

echo ">> wait for the standbys to apply the bootstrap write"
wait_replicated "$leader" 30 \
  || { echo "FAIL: a standby never replicated (applied_seq stayed 0)" >&2; exit 1; }

echo ">> partition the leader ($leader) with SIGSTOP"
kill -STOP "${PID[$leader]}"
STOPPED="${PID[$leader]}"
sleep 2  # let lease expire + election

echo ">> wait for a new leader during partition"
new_leader=$(wait_leader 60) || { echo "FAIL: no leader during partition" >&2; exit 1; }
echo "  new leader during partition=$new_leader"
[ "$new_leader" != "$leader" ] || { echo "FAIL: old leader still leading during partition" >&2; exit 1; }

echo ">> task completes via edge during partition"
task=$(create_task "partition smoke") || true
[ -n "$task" ] || { echo "FAIL: task create during partition" >&2; exit 1; }
ok=0
for _ in $(seq 1 120); do
  st=$(curl -fsS --max-time 15 --connect-timeout 5 -H "authorization: Bearer $jwt" "$EDGE_BASE/v1/tasks/$task" 2>/dev/null \
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
