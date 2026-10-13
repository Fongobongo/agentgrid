#!/usr/bin/env bash
# E2E (process-based, no Docker): the Secure-profile gate (Stage 12).
#
# A task with `security_profile: "default-strict"` (name ends in `-strict`)
# must NOT assign to a wrapper-only node — the scheduler reports
# "requires structured permission interception" — and MUST assign to an
# ACP/structured node, where it runs to `succeeded`. This is the executable
# half of the Secure profile (ADR 0015): strict tasks only land where tool
# calls are intercepted.
#
# Nodes: n1 = mock (wrapper), n2 = fake-acp:acp (structured; test ACP agent
# built with the workspace). Task adapter is fake-acp so both nodes are
# adapter-eligible and the ONLY discriminator is the strict gate.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

[ -x "$BIN/adapter-fake-acp" ] || { echo "build debug binaries first (adapter-fake-acp missing)"; exit 1; }

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7817}"
PORT="${AGENTGRID_PORT:-7817}"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-secure-secret"
E2E_CONCURRENCY=1

TMP="$(mktemp -d -t ag-e2e-secure-XXXXXX)"
mkdir -p "$TMP/n1" "$TMP/w1" "$TMP/r1" "$TMP/n2" "$TMP/w2" "$TMP/r2"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

submit_strict() {  # $1 = prompt; prints task id
  local prompt_json
  prompt_json=$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1")
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$prompt_json,\"repository\":\"*\",\"adapter\":\"fake-acp\",\"security_profile\":\"default-strict\",\"timeout_secs\":120}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}

echo ">> secure-profile gate: wrapper node first"
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node n1 "e2e-secure-wrapper" "mock" "$TMP/w1" "$TMP/r1" "$TMP/n1"
e2e_wait_nodes_online 1 || exit 1

echo ">> submitting default-strict task (wrapper node must refuse it)"
TID=$(submit_strict "secure e2e")
sleep 8
e2e_poll_status "$TID"
[ "$STATUS" = "queued" ] || { echo "  FAILED: strict task left queued on a wrapper node: $STATUS"; exit 1; }
echo "  still queued on wrapper-only fleet, as required"
curl -fsS "$BASE/v1/tasks/$TID/eligibility" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;d=json.load(sys.stdin);rs=[r for n in d.get("nodes",[]) for r in n.get("reasons",[])];assert any("structured" in r for r in rs),"no structured-interception reason: %r"%rs;print("  eligibility reason present:",[r for r in rs if "structured" in r][0])'

echo ">> adding structured (ACP) node: the same task must assign and succeed"
# Enrollment tokens are one-shot: n1 consumed the minted token, so a second
# node needs a freshly minted one (same pattern as run-cp-restart.sh).
e2e_mint_token
e2e_start_node n2 "e2e-secure-structured" "fake-acp:acp" "$TMP/w2" "$TMP/r2" "$TMP/n2"
e2e_wait_nodes_online 2 || { echo "--- nodes API:"; curl -s "$BASE/v1/nodes" -H "authorization: Bearer $jwt" || true; echo "--- n2 log:"; cat "$TMP/n2.log" || true; echo "--- cp log tail:"; tail -50 "$TMP/cp.log" || true; exit 1; }
e2e_wait_terminal 120 "$TID" || { echo "  task never terminal: $STATUS"; cat "$TMP/n2.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: expected succeeded on structured node, got $STATUS"; cat "$TMP/n2.log"; exit 1; }
echo "  strict task succeeded on the structured node"

echo ">> secure-profile gate OK (refused wrapper, ran structured)"
