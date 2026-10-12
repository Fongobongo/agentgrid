#!/usr/bin/env bash
# Plan 0.2 item 4.3: graceful drain E2E (process-based, no Docker).
#
# Scenario: submit a long task, drain the node mid-flight, then assert:
#   1. the in-flight task still completes `succeeded` (drain never kills work),
#   2. a task submitted while drained stays `queued` (no new assignments),
#   3. after undrain the queued task is assigned and succeeds.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7814}"
PORT="${AGENTGRID_PORT:-7814}"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-stable-secret"
E2E_CONCURRENCY=1

TMP="$(mktemp -d -t ag-e2e-drain-XXXXXX)"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node drain "e2e-drain-node" "mock" "$TMP/work" "$TMP/repos" "$TMP/node"
e2e_wait_nodes_online 1 || exit 1

NODE_ID=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["id"])')

echo ">> submitting long task sleep:8"
T1=$(e2e_submit "sleep:8" "mock" "*" 60)
e2e_wait_status running "$T1" 15 || { echo "task never reached running"; cat "$TMP/cp.log"; exit 1; }

echo ">> draining node $NODE_ID mid-flight"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/nodes/$NODE_ID/drain?drain=true" \
  -H "authorization: Bearer $jwt")
[ "$code" = "200" ] || { echo "drain returned $code"; exit 1; }

echo ">> submitting second task while drained (must stay queued)"
T2=$(e2e_submit "sleep:1" "mock" "*" 60)
sleep 6
e2e_poll_status "$T2"
[ "$STATUS" = "queued" ] || { echo "DRAIN FAILED: task assigned while drained (status=$STATUS)"; exit 1; }
echo "   second task still queued while drained"

echo ">> in-flight task must still complete"
e2e_wait_status succeeded "$T1" 30 || { echo "in-flight task ended $STATUS under drain"; cat "$TMP/drain.log"; exit 1; }
echo "   in-flight task succeeded under drain"

echo ">> undrain; queued task must now run"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/nodes/$NODE_ID/drain?drain=false" \
  -H "authorization: Bearer $jwt")
[ "$code" = "200" ] || { echo "undrain returned $code"; exit 1; }
e2e_wait_status succeeded "$T2" 30 || { echo "queued task ended $STATUS after undrain"; cat "$TMP/cp.log"; cat "$TMP/drain.log"; exit 1; }
echo "   queued task succeeded after undrain"

echo ">> E2E OK (drain: in-flight finishes, no new work, undrain resumes)"
