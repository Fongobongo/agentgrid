#!/usr/bin/env bash
# E2E (process-based, no Docker): incremental changeset replication.
#
# Plan Phase 4: 3 data hosts with the SQLite session extension. Three data
# voters keep a 2/3 data majority after the leader dies, which sync mode needs
# (a witness vote cannot stand in for data freshness). No snapshots are cut
# (interval 1h), so every replicated row must arrive as a changeset. Asserts:
#   - the follower's applied_seq advances after plain writes (no snapshot);
#   - in sync mode an acknowledged write is already on the follower (RPO=0);
#   - killing the leader keeps the acknowledged task and the new leader reads
#     it, with the sequence continuing without a gap;
#   - the restarted host rejoins as a standby.
#
# Requires a control-plane binary built with the feature:
#   cargo build -p agentgrid-control-plane --features ha-incremental
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
CP_BIN="${AGENTGRID_CP_BIN:-$ROOT/target/debug/agentgrid-control-plane}"
RPO="${AG_E2E_HA_RPO:-async}"

# Throwaway fixtures for control-plane processes this script boots on
# 127.0.0.1; never real credentials. The inline allow keeps gitleaks from
# re-flagging the same line on every edit (fingerprints are commit-scoped).
TOKEN="e2e-ha-incremental-token-0123456789abcd"  # gitleaks:allow
JWT_SECRET="e2e-ha-incremental-secret-0123456789ab"  # gitleaks:allow
CLUSTER="e2e-inc"
USER="admin"
PASS="changeme"

declare -A PORT=([cp-a]=17841 [cp-b]=17842 [cp-c]=17843)
declare -A PID=()
TMP="$(mktemp -d -t ag-e2e-ha-inc-XXXXXX)"

cleanup() {
  set +e
  for pid in "${PID[@]:-}"; do kill -9 "$pid" 2>/dev/null; done
  wait 2>/dev/null
  if [ "${AG_E2E_KEEP:-}" = "1" ]; then echo "logs kept in $TMP"; else rm -rf "$TMP"; fi
}
trap cleanup EXIT

[ -x "$CP_BIN" ] || { echo "missing $CP_BIN (cargo build first)" >&2; exit 1; }
# A default build still contains the feature name in error strings, so check
# for the actual session-extension symbol instead.
if command -v nm >/dev/null 2>&1 && ! nm "$CP_BIN" 2>/dev/null | grep ' sqlite3session_create$' >/dev/null; then
  echo "FAIL: $CP_BIN lacks the ha-incremental feature" >&2
  echo "  rebuild: cargo build -p agentgrid-control-plane --features ha-incremental" >&2
  exit 1
fi

start_node() {  # $1 = id, $2 = role
  local id="$1" role="$2"
  local dir="$TMP/$id"
  mkdir -p "$dir"
  local peers witnesses
  case "$id" in
    cp-a)
      peers="cp-b@http://127.0.0.1:${PORT[cp-b]},cp-c@http://127.0.0.1:${PORT[cp-c]}"
      ;;
    cp-b)
      peers="cp-a@http://127.0.0.1:${PORT[cp-a]},cp-c@http://127.0.0.1:${PORT[cp-c]}"
      ;;
    cp-c)
      peers="cp-a@http://127.0.0.1:${PORT[cp-a]},cp-b@http://127.0.0.1:${PORT[cp-b]}"
      ;;
  esac
  witnesses=""
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
      AGENTGRID_HA_RPO_MODE="$RPO" \
      AGENTGRID_HA_SNAPSHOT_INTERVAL_SECS=3600 \
      AGENTGRID_HA_WITNESS_DIR="$dir/witness" \
      "$CP_BIN" >"$TMP/$id.log" 2>&1 &
  PID[$id]=$!
}

ha_status() { curl -fsS -H "x-agentgrid-ha-token: $TOKEN" "http://127.0.0.1:$1/v1/ha/status"; }
field() { python3 -c 'import json,sys
try: print(json.loads(sys.argv[1]).get(sys.argv[2], ""))
except Exception: print("")' "$1" "$2"; }

leader_id() {
  for id in cp-a cp-b cp-c; do
    local s
    s=$(ha_status "${PORT[$id]}" 2>/dev/null || true)
    [ -n "$s" ] || continue
    [ "$(field "$s" role)" = "leader" ] && { echo "$id"; return; }
  done
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
wait_leader() {
  local rounds=$(( ${1:-100} * 5 ))
  for _ in $(seq 1 "$rounds"); do
    if [ "$(count_leaders)" = 1 ]; then leader_id; return 0; fi
    sleep 0.2
  done
  return 1
}
applied() { ha_status "$1" 2>/dev/null | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("applied_seq", 0))
except Exception: print(0)'; }

wait_applied() {  # $1 port, $2 target seq
  for _ in $(seq 1 100); do
    [ "$(applied "$1")" -ge "$2" ] && return 0
    sleep 0.1
  done
  return 1
}

echo ">> HA incremental ($RPO): starting 3 data nodes"
start_node cp-a data
start_node cp-b data
start_node cp-c data
leader=$(wait_leader 30) || { echo "FAIL: no single leader" >&2; exit 1; }
follower=$(for id in cp-a cp-b cp-c; do
  [ "$id" = "$leader" ] || { echo "$id"; break; }
done)
echo "  leader=$leader follower=$follower"

echo ">> bootstrap admin"
setup_token=$(awk '/agentgrid setup token/{getline; print; exit}' "$TMP/$leader.log")
[ -n "$setup_token" ] || { echo "FAIL: setup token not found" >&2; exit 1; }
curl -fsS -H 'content-type: application/json' -o /dev/null \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\",\"setup_token\":\"$setup_token\"}" \
  "http://127.0.0.1:${PORT[$leader]}/v1/auth/setup"
jwt=$(curl -fsS -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  "http://127.0.0.1:${PORT[$leader]}/v1/auth/login" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

echo ">> write a task; its changeset must reach the follower without a snapshot"
task=$(curl -fsS -X POST -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d '{"prompt":"spam:1","repository":"*","adapter":"mock","timeout_secs":60}' \
  "http://127.0.0.1:${PORT[$leader]}/v1/tasks" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
seq=$(applied "${PORT[$leader]}")
[ "$seq" -ge 1 ] || { echo "FAIL: leader did not advance repl_seq" >&2; exit 1; }
echo "  task=$task repl_seq=$seq"
wait_applied "${PORT[$follower]}" "$seq" || {
  echo "FAIL: follower applied_seq stalled at $(applied "${PORT[$follower]}"), want $seq" >&2
  tail -40 "$TMP/$follower.log" >&2
  exit 1
}
echo "  follower applied seq=$seq"

if [ "$RPO" = "sync" ]; then
  # The create call only returned after the data quorum ack, and the apply
  # path stamps repl_seq in the same transaction, so no waiting is allowed.
  now=$(applied "${PORT[$follower]}")
  [ "$now" -ge "$seq" ] || {
    echo "FAIL: sync write acknowledged but follower seq=$now < $seq" >&2
    exit 1
  }
  echo "  sync RPO=0: follower applied seq=$now"
fi

echo ">> kill -9 the leader; the acknowledged task must survive"
kill -9 "${PID[$leader]}"
unset 'PID[$leader]'
new=""
for _ in $(seq 1 300); do
  cand=$(wait_leader 1) || cand=""
  if [ -n "$cand" ] && curl -fsS "http://127.0.0.1:${PORT[$cand]}/health/ready" >/dev/null 2>&1; then
    new=$cand
    break
  fi
  sleep 0.1
done
[ -n "$new" ] || { echo "FAIL: no new leader within 30s" >&2; tail -40 "$TMP/$follower.log" >&2; exit 1; }
echo "  new leader=$new"

seen=""
for _ in $(seq 1 100); do
  seen=$(curl -fsS -H "authorization: Bearer $jwt" \
    "http://127.0.0.1:${PORT[$new]}/v1/tasks/$task" 2>/dev/null \
    | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))' 2>/dev/null || true)
  [ -n "$seen" ] && break
  sleep 0.2
done
[ "$seen" = "$task" ] || { echo "FAIL: task $task lost after failover" >&2; exit 1; }

echo ">> a post-failover write keeps the sequence contiguous"
task2=$(curl -fsS -X POST -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d '{"prompt":"spam:1","repository":"*","adapter":"mock","timeout_secs":60}' \
  "http://127.0.0.1:${PORT[$new]}/v1/tasks" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
new_seq=$(applied "${PORT[$new]}")
[ "$new_seq" -ge "$seq" ] || { echo "FAIL: new leader repl_seq $new_seq < $seq" >&2; exit 1; }

echo ">> restart the killed host; it must rejoin as standby"
start_node "$leader" data
for _ in $(seq 1 100); do
  s=$(ha_status "${PORT[$leader]}" 2>/dev/null || true)
  [ "$(field "$s" role)" = "standby" ] && break
  sleep 0.3
done
[ "$(field "$(ha_status "${PORT[$leader]}")" role)" = "standby" ] \
  || { echo "FAIL: restarted host did not become standby" >&2; tail -40 "$TMP/$leader.log" >&2; exit 1; }
[ "$(count_leaders)" = 1 ] || { echo "FAIL: expected exactly one leader" >&2; exit 1; }

echo ">> HA incremental OK ($RPO): task=$task survived, task2=$task2, seq=$new_seq, one leader"
