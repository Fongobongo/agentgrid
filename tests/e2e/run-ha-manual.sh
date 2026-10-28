#!/usr/bin/env bash
# E2E (process-based, no Docker): control-plane HA manual failover.
#
# Plan Phase 2: 2 data hosts + 1 witness, election, snapshot replication,
# manual promotion, and fencing of the old leader. Asserts:
#   - exactly one leader after election;
#   - a standby answers data routes with 503 + role/leader headers;
#   - a snapshot produced by the leader restores into the standby;
#   - manual promotion makes the standby writable at a higher epoch;
#   - the old leader steps down and its writes/routes are fenced;
#   - state created before failover (the admin user) survives on the new leader.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
CP_BIN="${AGENTGRID_CP_BIN:-$ROOT/target/debug/agentgrid-control-plane}"

TOKEN="e2e-ha-cluster-token-0123456789abcdef"
JWT_SECRET="e2e-ha-jwt-secret-0123456789abcdef0123456789"
CLUSTER="e2e"
USER="admin"
PASS="changeme"

declare -A PORT=([cp-a]=17811 [cp-b]=17812 [w1]=17813)
TMP="$(mktemp -d -t ag-e2e-ha-XXXXXX)"
PIDS=()

cleanup() {
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  if [ "${AG_E2E_KEEP:-}" = "1" ]; then
    echo "logs kept in $TMP"
  else
    rm -rf "$TMP"
  fi
}
trap cleanup EXIT

if [ ! -x "$CP_BIN" ]; then
  echo "missing $CP_BIN (build with: cargo build -p agentgrid-control-plane)" >&2
  exit 1
fi

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
  PIDS+=("$!")
}

ha_status() {  # $1 = port
  curl -fsS -H "x-agentgrid-ha-token: $TOKEN" "http://127.0.0.1:$1/v1/ha/status"
}

field() {  # $1 = json, $2 = key
  python3 -c 'import json,sys; print(json.loads(sys.argv[1]).get(sys.argv[2], ""))' "$1" "$2"
}

count_leaders() {
  local n=0
  for id in cp-a cp-b; do
    local s
    s=$(ha_status "${PORT[$id]}" 2>/dev/null || true)
    [ -n "$s" ] || continue
    if [ "$(field "$s" role)" = "leader" ]; then n=$((n + 1)); fi
  done
  echo "$n"
}

leader_id() {
  for id in cp-a cp-b; do
    local s
    s=$(ha_status "${PORT[$id]}" 2>/dev/null || true)
    [ -n "$s" ] || continue
    if [ "$(field "$s" role)" = "leader" ]; then echo "$id"; return; fi
  done
}

other_id() {  # $1 = leader id
  [ "$1" = cp-a ] && echo cp-b || echo cp-a
}

echo ">> HA manual failover: starting witness + 2 data nodes"
start_node w1 witness
start_node cp-a data
start_node cp-b data

echo "  waiting for election (quorum 2/3)"
leader=""
for _ in $(seq 1 100); do
  if [ "$(count_leaders)" = 1 ]; then leader=$(leader_id); break; fi
  sleep 0.3
done
if [ -z "$leader" ]; then
  echo "FAIL: no single leader after 30s" >&2
  tail -n 40 "$TMP/cp-a.log" >&2 || true
  tail -n 40 "$TMP/cp-b.log" >&2 || true
  exit 1
fi
follower=$(other_id "$leader")
echo "  leader=$leader follower=$follower"

echo ">> standby must refuse data routes with 503 + leader hint"
code=$(curl -s -o "$TMP/standby.json" -D "$TMP/standby.hdr" -w '%{http_code}' \
  "http://127.0.0.1:${PORT[$follower]}/v1/tasks")
[ "$code" = "503" ] || { echo "FAIL: standby /v1/tasks -> $code" >&2; exit 1; }
grep -qi '^x-agentgrid-role: standby' "$TMP/standby.hdr" \
  || { echo "FAIL: standby response lacks x-agentgrid-role" >&2; cat "$TMP/standby.hdr" >&2; exit 1; }
grep -qi "^x-agentgrid-leader: .*${PORT[$leader]}" "$TMP/standby.hdr" \
  || { echo "FAIL: standby response lacks the leader hint" >&2; cat "$TMP/standby.hdr" >&2; exit 1; }

echo ">> bootstrap admin on the leader"
setup_token=$(awk '/agentgrid setup token/{getline; print; exit}' "$TMP/$leader.log")
[ -n "$setup_token" ] || { echo "FAIL: setup token not found in $leader log" >&2; exit 1; }
code=$(curl -s -o "$TMP/setup.json" -w '%{http_code}' -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\",\"setup_token\":\"$setup_token\"}" \
  "http://127.0.0.1:${PORT[$leader]}/v1/auth/setup")
[ "$code" = "200" ] || [ "$code" = "201" ] \
  || { echo "FAIL: setup -> $code: $(cat "$TMP/setup.json")" >&2; exit 1; }

echo ">> leader writes a snapshot; standby replicates it"
snap=$(curl -fsS -X POST -H "x-agentgrid-ha-token: $TOKEN" \
  "http://127.0.0.1:${PORT[$leader]}/v1/ha/snapshot") \
  || { echo "FAIL: snapshot request" >&2; exit 1; }
echo "  snapshot: $(field "$snap" snapshot_id) seq=$(field "$snap" seq)"

echo ">> manual promotion of $follower"
promote=$(curl -fsS -X POST -H "x-agentgrid-ha-token: $TOKEN" \
  "http://127.0.0.1:${PORT[$follower]}/v1/ha/promote") \
  || { echo "FAIL: promote request" >&2; exit 1; }
[ "$(field "$promote" role)" = "leader" ] \
  || { echo "FAIL: promote returned $(cat "$promote")" >&2; exit 1; }
new_epoch=$(field "$promote" epoch)
[ "$new_epoch" -ge 2 ] || { echo "FAIL: epoch did not advance ($new_epoch)" >&2; exit 1; }
echo "  $follower is leader at epoch $new_epoch"

echo ">> replicated state is visible on the new leader (login as $USER)"
code=$(curl -s -o "$TMP/login.json" -w '%{http_code}' -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  "http://127.0.0.1:${PORT[$follower]}/v1/auth/login")
[ "$code" = "200" ] || { echo "FAIL: login on new leader -> $code: $(cat "$TMP/login.json")" >&2; exit 1; }

echo ">> old leader steps down and is fenced"
for _ in $(seq 1 50); do
  s=$(ha_status "${PORT[$leader]}" 2>/dev/null || true)
  [ "$(field "$s" role)" = "standby" ] && break
  sleep 0.3
done
s=$(ha_status "${PORT[$leader]}" 2>/dev/null || true)
[ "$(field "$s" role)" = "standby" ] \
  || { echo "FAIL: old leader did not demote: $(cat "$s")" >&2; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${PORT[$leader]}/v1/tasks")
[ "$code" = "503" ] || { echo "FAIL: old leader /v1/tasks -> $code (expected 503)" >&2; exit 1; }

[ "$(count_leaders)" = 1 ] || { echo "FAIL: expected exactly one leader" >&2; exit 1; }

echo ">> HA manual failover OK: one leader, state replicated, old leader fenced"
