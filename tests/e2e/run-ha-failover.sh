#!/usr/bin/env bash
# E2E (process-based, no Docker): control-plane HA automated failover.
#
# Plan Phase 3: 2 data hosts + 1 witness. Starts one node-daemon configured
# with both control-plane URLs (`AGENTGRID_SERVERS`), runs a task, hard-kills
# the leader (`kill -9`), and asserts:
#   - a remaining data host wins the election and becomes ready;
#   - RTO (kill -> new leader ready) stays under the 60s target;
#   - the node follows the new leader and the in-flight task completes;
#   - a new task submitted after failover succeeds;
#   - the killed host restarts as a standby, never a second leader;
#   - a graceful demote closes WS with 4004 and the node reconnects.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
CP_BIN="${AGENTGRID_CP_BIN:-$ROOT/target/debug/agentgrid-control-plane}"
NODE_BIN="${AGENTGRID_NODE_BIN:-$ROOT/target/debug/agentgrid-node-daemon}"
ADAPTER_BIN="$ROOT/target/debug"

TOKEN="e2e-ha-cluster-token-0123456789abcdef"
JWT_SECRET="e2e-ha-jwt-secret-0123456789abcdef0123456789"
CLUSTER="e2e"
USER="admin"
PASS="changeme"

declare -A PORT=([cp-a]=17821 [cp-b]=17822 [w1]=17823)
declare -A PID=()
TMP="$(mktemp -d -t ag-e2e-ha-failover-XXXXXX)"
NODE_PID=""

cleanup() {
  set +e
  [ -n "$NODE_PID" ] && kill -9 "$NODE_PID" 2>/dev/null
  for pid in "${PID[@]:-}"; do kill -9 "$pid" 2>/dev/null; done
  wait 2>/dev/null
  if [ "${AG_E2E_KEEP:-}" = "1" ]; then echo "logs kept in $TMP"; else rm -rf "$TMP"; fi
}
trap cleanup EXIT

for b in "$CP_BIN" "$NODE_BIN" "$ADAPTER_BIN/adapter-mock"; do
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

ha_status() { curl -fsS -H "x-agentgrid-ha-token: $TOKEN" "http://127.0.0.1:$1/v1/ha/status"; }
field() {  # $1 = json, $2 = key
  python3 -c 'import json,sys
try: print(json.loads(sys.argv[1]).get(sys.argv[2], ""))
except Exception: print("")' "$1" "$2"
}

leader_id() {
  for id in cp-a cp-b; do
    local s
    s=$(ha_status "${PORT[$id]}" 2>/dev/null || true)
    [ -n "$s" ] || continue
    [ "$(field "$s" role)" = "leader" ] && { echo "$id"; return; }
  done
}
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
other_id() { [ "$1" = cp-a ] && echo cp-b || echo cp-a; }

wait_leader() {  # [timeout_secs]
  local rounds=$(( ${1:-100} * 5 ))
  for _ in $(seq 1 "$rounds"); do
    if [ "$(count_leaders)" = 1 ]; then leader_id; return 0; fi
    sleep 0.2
  done
  return 1
}

echo ">> HA failover: starting witness + 2 data nodes"
start_node w1 witness
start_node cp-a data
start_node cp-b data
leader=$(wait_leader 30) || { echo "FAIL: no single leader" >&2; exit 1; }
echo "  initial leader=$leader"

echo ">> bootstrap admin + mint enrollment token"
setup_token=$(awk '/agentgrid setup token/{getline; print; exit}' "$TMP/$leader.log")
[ -n "$setup_token" ] || { echo "FAIL: setup token not found" >&2; exit 1; }
curl -fsS -H 'content-type: application/json' -o /dev/null \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\",\"setup_token\":\"$setup_token\"}" \
  "http://127.0.0.1:${PORT[$leader]}/v1/auth/setup"
jwt=$(curl -fsS -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  "http://127.0.0.1:${PORT[$leader]}/v1/auth/login" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
enroll=$(curl -fsS -X POST -H "authorization: Bearer $jwt" \
  "http://127.0.0.1:${PORT[$leader]}/v1/nodes/enrollment-token" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

echo ">> starting node-daemon with both control-plane URLs"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos"
env PATH="$ADAPTER_BIN:$PATH" \
    RUST_LOG=info \
    AGENTGRID_SERVER="http://127.0.0.1:${PORT[$leader]}" \
    AGENTGRID_SERVERS="http://127.0.0.1:${PORT[cp-a]},http://127.0.0.1:${PORT[cp-b]}" \
    AGENTGRID_DATA_DIR="$TMP/node" \
    AGENTGRID_NODE_NAME="ha-node" \
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
  curl -fsS -H "authorization: Bearer $jwt" "$1/v1/nodes" 2>/dev/null \
    | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(sum(1 for x in ns if x.get("status")=="online"))' 2>/dev/null || echo 0
}
for _ in $(seq 1 100); do
  [ "$(node_online "http://127.0.0.1:${PORT[$leader]}")" = "1" ] && break
  sleep 0.5
done
[ "$(node_online "http://127.0.0.1:${PORT[$leader]}")" = "1" ] \
  || { echo "FAIL: node did not come online" >&2; tail -40 "$TMP/node.log" >&2; exit 1; }
echo "  node online"

echo ">> a quick task uploads an artifact while the initial leader is alive"
pre=$(curl -fsS -X POST -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d '{"prompt":"write:out.txt:hello\nspam:5","repository":"*","adapter":"mock","timeout_secs":60}' \
  "http://127.0.0.1:${PORT[$leader]}/v1/tasks" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
pre_status=""
for _ in $(seq 1 120); do
  pre_status=$(curl -fsS -H "authorization: Bearer $jwt" \
    "http://127.0.0.1:${PORT[$leader]}/v1/tasks/$pre" 2>/dev/null \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])' 2>/dev/null || true)
  [ "$pre_status" = "succeeded" ] && break
  sleep 0.5
done
[ "$pre_status" = "succeeded" ] || { echo "FAIL: pre-failover task -> '$pre_status'" >&2; exit 1; }
echo "  pre-failover task=$pre succeeded"

echo ">> submit a long task and wait until it runs"
task=$(curl -fsS -X POST -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d '{"prompt":"spam:200\nsleep:20","repository":"*","adapter":"mock","timeout_secs":120}' \
  "http://127.0.0.1:${PORT[$leader]}/v1/tasks" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
echo "  task=$task"
status() {  # $1 = base url
  curl -fsS -H "authorization: Bearer $jwt" "$1/v1/tasks/$task" 2>/dev/null \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])' 2>/dev/null || true
}
for _ in $(seq 1 60); do
  [ "$(status "http://127.0.0.1:${PORT[$leader]}")" = "running" ] && break
  sleep 0.3
done
[ "$(status "http://127.0.0.1:${PORT[$leader]}")" = "running" ] \
  || { echo "FAIL: task never reached running" >&2; exit 1; }

echo ">> force a snapshot so the running assignment is replicated"
curl -fsS -X POST -H "x-agentgrid-ha-token: $TOKEN" \
  "http://127.0.0.1:${PORT[$leader]}/v1/ha/snapshot" >/dev/null
follower=$(other_id "$leader")
for _ in $(seq 1 50); do
  s=$(ha_status "${PORT[$follower]}" 2>/dev/null || true)
  [ -n "$s" ] && [ "$(field "$s" applied_seq)" -ge 1 ] && break
  sleep 0.3
done

echo ">> kill -9 leader ($leader); measuring RTO"
killed=$leader
t0=$(date +%s%N)
kill -9 "${PID[$killed]}"
unset 'PID[$killed]'
new=""
for _ in $(seq 1 300); do
  cand=$(wait_leader 1) || cand=""
  if [ -n "$cand" ] && curl -fsS "http://127.0.0.1:${PORT[$cand]}/health/ready" >/dev/null 2>&1; then
    new=$cand
    break
  fi
  sleep 0.1
done
t1=$(date +%s%N)
[ -n "$new" ] || { echo "FAIL: no new leader within 30s" >&2; tail -40 "$TMP/$follower.log" >&2; exit 1; }
rto_ms=$(( (t1 - t0) / 1000000 ))
echo "  new leader=$new at epoch $(field "$(ha_status "${PORT[$new]}")" epoch), RTO=${rto_ms}ms"
[ "$rto_ms" -lt 60000 ] || { echo "FAIL: RTO ${rto_ms}ms exceeds 60s target" >&2; exit 1; }

echo ">> node follows the new leader and the task completes"
for _ in $(seq 1 120); do
  [ "$(node_online "http://127.0.0.1:${PORT[$new]}")" = "1" ] && break
  sleep 0.5
done
[ "$(node_online "http://127.0.0.1:${PORT[$new]}")" = "1" ] \
  || { echo "FAIL: node did not reconnect to $new" >&2; tail -60 "$TMP/node.log" >&2; exit 1; }
final=""
for _ in $(seq 1 120); do
  final=$(status "http://127.0.0.1:${PORT[$new]}")
  case "$final" in succeeded|failed|cancelled|timed_out|lost) break;; esac
  sleep 1
done
[ "$final" = "succeeded" ] \
  || { echo "FAIL: in-flight task ended as '$final'" >&2; tail -60 "$TMP/node.log" >&2; exit 1; }
echo "  task $task succeeded after failover"

echo ">> a new task on the new leader succeeds"
task2=$(curl -fsS -X POST -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d '{"prompt":"spam:10","repository":"*","adapter":"mock","timeout_secs":60}' \
  "http://127.0.0.1:${PORT[$new]}/v1/tasks" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
final=""
for _ in $(seq 1 60); do
  final=$(curl -fsS -H "authorization: Bearer $jwt" "http://127.0.0.1:${PORT[$new]}/v1/tasks/$task2" 2>/dev/null \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])' 2>/dev/null || true)
  [ "$final" = "succeeded" ] && break
  sleep 0.5
done
[ "$final" = "succeeded" ] || {
  echo "FAIL: post-failover task -> '$final'" >&2
  curl -fsS -H "authorization: Bearer $jwt" "http://127.0.0.1:${PORT[$new]}/v1/nodes" >&2 || true
  echo >&2
  curl -fsS -H "authorization: Bearer $jwt" "http://127.0.0.1:${PORT[$new]}/v1/tasks/$task2" >&2 || true
  echo >&2
  exit 1
}
echo "  post-failover task succeeded"

echo ">> restart the killed host; it must rejoin as standby, never a second leader"
start_node "$killed" data
for _ in $(seq 1 100); do
  s=$(ha_status "${PORT[$killed]}" 2>/dev/null || true)
  [ "$(field "$s" role)" = "standby" ] && break
  sleep 0.3
done
[ "$(field "$(ha_status "${PORT[$killed]}")" role)" = "standby" ] \
  || { echo "FAIL: restarted host did not become standby" >&2; tail -40 "$TMP/$killed.log" >&2; exit 1; }
[ "$(count_leaders)" = 1 ] || { echo "FAIL: expected exactly one leader after rejoin" >&2; exit 1; }

echo ">> artifact bytes are pullable by the new leader after failover"
arts=$(curl -fsS -H "authorization: Bearer $jwt" \
  "http://127.0.0.1:${PORT[$new]}/v1/tasks/$pre/artifacts")
name=$(echo "$arts" | python3 -c 'import sys,json; a=json.load(sys.stdin); print(a[0]["name"] if a else "")')
[ -n "$name" ] || { echo "FAIL: task published no artifacts" >&2; exit 1; }
# Drop the leader's local copies so the read must fetch + verify from a peer.
rm -rf "$TMP/$new/artifacts"
bytes=$(curl -fsS -H "authorization: Bearer $jwt" \
  "http://127.0.0.1:${PORT[$new]}/v1/tasks/$pre/artifacts/$name" | wc -c)
[ "$bytes" -gt 0 ] || { echo "FAIL: artifact $name not pulled from a peer" >&2; exit 1; }
echo "  $name ($bytes bytes) pulled and served"

echo ">> graceful demote: WS 4004, node reconnects, cluster elects a new leader"
curl -fsS -X POST -H "x-agentgrid-ha-token: $TOKEN" \
  "http://127.0.0.1:${PORT[$new]}/v1/ha/demote" >/dev/null
grep -q "ws: demoted, reconnecting" "$TMP/node.log" \
  || { echo "FAIL: node did not see the 4004 demote close" >&2; tail -40 "$TMP/node.log" >&2; exit 1; }
new2=$(wait_leader 30) || { echo "FAIL: no leader after demote" >&2; exit 1; }
echo "  leader after demote=$new2"
for _ in $(seq 1 120); do
  [ "$(node_online "http://127.0.0.1:${PORT[$new2]}")" = "1" ] && break
  sleep 0.5
done
[ "$(node_online "http://127.0.0.1:${PORT[$new2]}")" = "1" ] \
  || { echo "FAIL: node did not reconnect after 4004 demote" >&2; tail -60 "$TMP/node.log" >&2; exit 1; }
task3=$(curl -fsS -X POST -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d '{"prompt":"spam:5","repository":"*","adapter":"mock","timeout_secs":60}' \
  "http://127.0.0.1:${PORT[$new2]}/v1/tasks" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
final=""
for _ in $(seq 1 60); do
  final=$(curl -fsS -H "authorization: Bearer $jwt" "http://127.0.0.1:${PORT[$new2]}/v1/tasks/$task3" 2>/dev/null \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])' 2>/dev/null || true)
  [ "$final" = "succeeded" ] && break
  sleep 0.5
done
[ "$final" = "succeeded" ] || { echo "FAIL: post-demote task -> '$final'" >&2; exit 1; }

[ "$(count_leaders)" = 1 ] || { echo "FAIL: expected exactly one leader at the end" >&2; exit 1; }
echo ">> HA failover OK: RTO=${rto_ms}ms, task survived, node followed, one leader"
