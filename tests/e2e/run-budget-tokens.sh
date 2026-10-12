#!/usr/bin/env bash
# E2E (process-based, no Docker, no LLM): cost tracking end to end.
#
# A mock worker emits a deterministic `usage:1000:5` metric; a workflow
# template budgeted at 500 tokens must park `Blocked` with a `max_tokens`
# breach, and /metrics must expose the fleet token/cost totals. Proves the
# full chain without any LLM: adapter progress event → node Metric ingest →
# CP aggregation → budget enforcement → Prometheus counters.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7822}"
PORT="${AGENTGRID_PORT:-7822}"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-cost-secret"
E2E_CONCURRENCY=2

TMP="$(mktemp -d -t ag-e2e-cost-XXXXXX)"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

echo ">> cost-tracking E2E: mock usage must trip a token budget"
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node cost "e2e-cost" "mock" "$TMP/work" "$TMP/repos" "$TMP/node"
e2e_wait_nodes_online 1 || exit 1

echo ">> template with a 500-token budget, worker reports 1000"
DEF=$(python3 -c 'import json;print(json.dumps({"name":"e2e-cost","steps":[
  {"id":"w","prompt":"usage:1000:5","role":"worker","depends_on":[]},
  {"id":"v","prompt":"verify","role":"verifier","depends_on":["w"]}],
  "budget":{"max_tokens":500}}))')
TID=$(curl -fsS -X POST "$BASE/v1/workflows" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' -d "$DEF" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
RID=$(curl -fsS -X POST "$BASE/v1/workflows/$TID/runs" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' -d '{"repository":"*"}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

echo ">> ticking to terminal (expect Blocked on max_tokens)"
status=""
for _ in $(seq 1 150); do
  curl -fsS -X POST "$BASE/v1/workflow-runs/$RID/tick" -H "authorization: Bearer $jwt" >/dev/null
  status=$(curl -fsS "$BASE/v1/workflow-runs/$RID" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["run"]["status"])')
  case "$status" in
    succeeded|failed|cancelled|blocked) break;;
  esac
  sleep 2
done
[ "$status" = "blocked" ] || { echo "  FAILED: run $RID -> $status (want blocked)"; exit 1; }
echo "  run blocked as budgeted"

echo ">> breach field must be max_tokens with observed >= 1000"
curl -fsS "$BASE/v1/workflow-runs/$RID/projection" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;b=json.load(sys.stdin)["budget"];br=b["breach"];assert br["field"]=="max_tokens",(br,);assert br["observed"]>=1000,(br,);assert b["usage"]["tokens"]>=1000,(b,);print("  breach:",br)'
echo "  budget snapshot agrees"

echo ">> /metrics must expose fleet token/cost totals"
METS=$(curl -fsS "$BASE/metrics")
echo "$METS" | grep -q "^agentgrid_llm_tokens_total [1-9]" || { echo "  FAILED: no token total in metrics"; exit 1; }
echo "$METS" | grep -q "^agentgrid_llm_cost_cents_total [1-9]" || { echo "  FAILED: no cost total in metrics"; exit 1; }
echo "  metrics counters present"

echo ">> cost-tracking E2E OK (usage in, breach out, counters up)"
