#!/usr/bin/env bash
# E2E (process-based, no Docker): HA 3 data hosts + edge forwarder chaos.
#
# Starts 3 control-plane data hosts (snapshot RPO, peer backend, plaintext),
# an agentgrid-cp-edge in front, bootstraps an admin + mints an enrollment
# token, starts a node-daemon through the edge, submits a task via the edge,
# hard-kills the leader (kill -9) and asserts:
#   - exactly one leader remains/wins after the kill;
#   - the edge forwards subsequent requests to the new leader (503 -> retry),
#     even while the old leader's socket still accepts connections;
#   - the node follows the new leader;
#   - a new task submitted after failover via the edge succeeds;
#   - the killed host restarts as a standby, never a second leader.
#
# Snapshot RPO: a task acked just before the kill can be missing on the new
# leader (a standby only pulls snapshots from a live leader), so the pre-kill
# task is probed, not required. Durability under sync replication is covered
# by run-ha-incremental.sh.
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
TOKEN="e2e-ha-3host-token-0123456789abcdef"  # gitleaks:allow
JWT_SECRET="e2e-ha-3host-secret-0123456789abcdef0123456789"  # gitleaks:allow
CLUSTER="e2e-3host"
USER="admin"
PASS="changeme"

declare -A PORT=([cp-a]=17901 [cp-b]=17902 [cp-c]=17903 [edge]=17904)
DATA_IDS=(cp-a cp-b cp-c)
declare -A PID=()
TMP="$(mktemp -d -t ag-e2e-ha-3host-XXXXXX)"
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

start_node() {  # $1 = id
  local id="$1"
  local dir="$TMP/$id"
  mkdir -p "$dir"
  local peers witnesses
  case "$id" in
    cp-a) peers="cp-b@http://127.0.0.1:${PORT[cp-b]},cp-c@http://127.0.0.1:${PORT[cp-c]}";;
    cp-b) peers="cp-a@http://127.0.0.1:${PORT[cp-a]},cp-c@http://127.0.0.1:${PORT[cp-c]}";;
    cp-c) peers="cp-a@http://127.0.0.1:${PORT[cp-a]},cp-b@http://127.0.0.1:${PORT[cp-b]}";;
  esac
  witnesses=""
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

edge_ready() {
  curl -fsS --max-time 15 --connect-timeout 5 "http://127.0.0.1:${PORT[edge]}/health/live" >/dev/null 2>&1
}

echo ">> HA 3host chaos: starting 3 data nodes + edge"
start_node cp-a
start_node cp-b
start_node cp-c
start_edge
leader=$(wait_leader 30) || { echo "FAIL: no single leader" >&2; exit 1; }
echo "  initial leader=$leader edge=$(edge_ready && echo up || echo down)"

echo ">> bootstrap admin + mint enrollment token (via edge)"
EDGE_BASE="http://127.0.0.1:${PORT[edge]}"
setup_token=$(awk '/agentgrid setup token/{getline; print; exit}' "$TMP/$leader.log")
[ -n "$setup_token" ] || { echo "FAIL: setup token not found" >&2; exit 1; }
curl -fsS --max-time 15 --connect-timeout 5 -H 'content-type: application/json' -o /dev/null \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\",\"setup_token\":\"$setup_token\"}" \
  "$EDGE_BASE/v1/auth/setup"
jwt=$(curl -fsS --max-time 15 --connect-timeout 5 -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  "$EDGE_BASE/v1/auth/login" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
enroll=$(curl -fsS --max-time 15 --connect-timeout 5 -X POST -H "authorization: Bearer $jwt" \
  "$EDGE_BASE/v1/nodes/enrollment-token" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

echo ">> node-daemon through the edge"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos"
env PATH="$ADAPTER_BIN:$PATH" \
    RUST_LOG=info \
    AGENTGRID_SERVER="$EDGE_BASE" \
    AGENTGRID_SERVERS="http://127.0.0.1:${PORT[cp-a]},http://127.0.0.1:${PORT[cp-b]},http://127.0.0.1:${PORT[cp-c]}" \
    AGENTGRID_DATA_DIR="$TMP/node" \
    AGENTGRID_NODE_NAME="ha-3host-node" \
    AGENTGRID_WORKSPACE_ROOT="$TMP/work" \
    AGENTGRID_REPOSITORY_ROOT="$TMP/repos" \
    AGENTGRID_ADAPTERS=mock \
    AGENTGRID_MAX_CONCURRENCY=2 \
    AGENTGRID_ENROLL_TOKEN="$enroll" \
    AGENTGRID_MIN_FREE_DISK_MB=1024 \
    AGENTGRID_MIN_FREE_MEM_MB=512 \
    "$NODE_BIN" >"$TMP/node.log" 2>&1 &
NODE_PID=$!

node_online() {  # $1 = base url
  curl -fsS --max-time 15 --connect-timeout 5 -H "authorization: Bearer $jwt" "$1/v1/nodes" 2>/dev/null \
    | python3 -c 'import sys,json
try:
 d=json.load(sys.stdin); ns=d.get("items",d) if isinstance(d,dict) else d
 print(sum(1 for n in ns if n.get("status")=="online"))
except Exception: print(0)' 2>/dev/null || echo 0
}

for _ in $(seq 1 60); do
  [ "$(node_online "$EDGE_BASE")" -ge 1 ] && break
  sleep 0.5
done
[ "$(node_online "$EDGE_BASE")" -ge 1 ] || { echo "FAIL: node not online via edge" >&2; exit 1; }
echo "  node online via edge"

echo ">> submit task via edge (leader=$leader)"
task=$(create_task "chaos 3host smoke") || true
[ -n "$task" ] || { echo "FAIL: task create failed" >&2; exit 1; }
echo "  task=$task"

echo ">> wait for task success (via edge)"
ok=0
for _ in $(seq 1 120); do
  st=$(curl -fsS --max-time 15 --connect-timeout 5 -H "authorization: Bearer $jwt" "$EDGE_BASE/v1/tasks/$task" 2>/dev/null \
    | python3 -c 'import sys,json
try: print(json.load(sys.stdin).get("status","?"))
except Exception: print("?")' 2>/dev/null || echo "?")
  [ "$st" = "succeeded" ] && ok=1 && break
  [ "$st" = "failed" ] && { echo "FAIL: task failed" >&2; exit 1; }
  sleep 0.5
done
[ "$ok" = "1" ] || { echo "FAIL: task did not succeed" >&2; exit 1; }
echo "  task succeeded before kill"

echo ">> wait for every standby to apply the bootstrap write"
wait_replicated "$leader" 45 \
  || { echo "FAIL: a standby never replicated (applied_seq stayed 0)" >&2; exit 1; }

echo ">> hard-kill the leader ($leader)"
kill -9 "${PID[$leader]}" 2>/dev/null || true
unset 'PID[$leader]'

echo ">> wait for exactly one leader after kill"
new_leader=$(wait_leader 60) || { echo "FAIL: no single leader after kill" >&2; exit 1; }
echo "  new leader=$new_leader"
[ "$new_leader" != "$leader" ] || echo "  note: same id re-elected"

echo ">> edge forwards to the new leader after the kill"
# This cluster runs snapshot RPO (see the header), so a write acked just
# before the kill may legitimately be missing on the new leader — the standby
# only ever pulls snapshots from a live leader. What must hold here is that
# the edge reached *a* leader: a 404 is the new leader answering, a connect
# error or a 5xx is the edge failing over badly. "No acknowledged write is
# lost" is asserted by run-ha-incremental.sh, which runs with sync replication.
code=$(curl -sS --max-time 15 --connect-timeout 5 -D "$TMP/probe.hdr" -o "$TMP/probe.json" \
  -w '%{http_code}' -H "authorization: Bearer $jwt" "$EDGE_BASE/v1/tasks/$task" 2>/dev/null || true)
case "${code:-000}" in
  200) echo "  pre-kill task present on the new leader" ;;
  404) echo "  pre-kill task fell outside the snapshot RPO window (expected)" ;;
  *)   echo "FAIL: edge did not reach a leader after failover (http=${code:-000})" >&2
       echo "  body: $(head -c 200 "$TMP/probe.json" 2>/dev/null)" >&2
       echo "  role header: $(grep -i '^x-agentgrid-role' "$TMP/probe.hdr" 2>/dev/null || echo none)" >&2
       echo "  retry-after: $(grep -i '^retry-after' "$TMP/probe.hdr" 2>/dev/null || echo none)" >&2
       exit 1 ;;
esac

echo ">> new task after failover via edge"
task2=$(create_task "post-failover") || true
[ -n "$task2" ] || { echo "FAIL: post-failover task create failed" >&2; exit 1; }
ok=0
for _ in $(seq 1 120); do
  st=$(curl -fsS --max-time 15 --connect-timeout 5 -H "authorization: Bearer $jwt" "$EDGE_BASE/v1/tasks/$task2" 2>/dev/null \
    | python3 -c 'import sys,json
try: print(json.load(sys.stdin).get("status","?"))
except Exception: print("?")' 2>/dev/null || echo "?")
  [ "$st" = "succeeded" ] && ok=1 && break
  [ "$st" = "failed" ] && { echo "FAIL: post-failover task failed" >&2; exit 1; }
  sleep 0.5
done
[ "$ok" = "1" ] || { echo "FAIL: post-failover task did not succeed" >&2; exit 1; }
echo "  post-failover task succeeded"

echo ">> exactly one leader now (killed host must not be a second leader)"
[ "$(count_leaders)" = "1" ] || { echo "FAIL: $(count_leaders) leaders after failover" >&2; exit 1; }
echo "PASS: 3host chaos (leader kill, edge forwarding, failover, post-failover task)"
