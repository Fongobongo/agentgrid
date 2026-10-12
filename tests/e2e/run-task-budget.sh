#!/usr/bin/env bash
# E2E (process-based, no Docker, no LLM): per-task cost ceilings.
#
# A mock task reporting `usage:500` with `max_tokens: 100` must be killed
# mid-run with `error_code = "budget_exceeded"` (failed, not cancelled),
# leaving a `budget_exceeded` error event with the observed totals. A second
# task with no ceilings but identical usage must succeed — proving the kill
# came from the ceiling, not the usage event itself.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7823}"
PORT="${AGENTGRID_PORT:-7823}"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-taskbudget-secret"
E2E_CONCURRENCY=1

TMP="$(mktemp -d -t ag-e2e-taskbudget-XXXXXX)"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

submit_capped() {  # $1 = prompt, $2 = max_tokens JSON (number or null)
  local prompt_json
  prompt_json=$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1")
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$prompt_json,\"repository\":\"*\",\"adapter\":\"mock\",\"timeout_secs\":120,\"max_tokens\":$2}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}

echo ">> task-budget E2E: ceiling kills, no-ceiling succeeds"
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node tb "e2e-taskbudget" "mock" "$TMP/work" "$TMP/repos" "$TMP/node"
e2e_wait_nodes_online 1 || exit 1

echo ">> capped task (usage:500 vs max_tokens:100) must fail budget_exceeded"
# usage first, then sleep: the 1s budget poll must observe the totals while
# the process is still alive (a usage-then-exit task could win the race).
T1=$(submit_capped $'usage:500:5\nsleep:5' 100)
e2e_wait_terminal 120 "$T1" || { echo "  task never terminal: $STATUS"; cat "$TMP/tb.log"; exit 1; }
[ "$STATUS" = "failed" ] || { echo "  FAILED: expected failed, got $STATUS"; exit 1; }
e2e_task_events "$T1" \
  | python3 -c 'import sys,json;evs=json.load(sys.stdin);hits=[e for e in evs if e.get("payload",{}).get("error_code")=="budget_exceeded"];assert hits,"no budget_exceeded event: %d events"%len(evs);print("  budget_exceeded event present")'
curl -fsS "$BASE/v1/tasks/$T1" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;d=json.load(sys.stdin);assert d.get("error_code")=="budget_exceeded",(d,);print("  task error_code=budget_exceeded")'

echo ">> uncapped task with identical usage must succeed"
T2=$(submit_capped $'usage:500:5\nsleep:5' null)
e2e_wait_terminal 120 "$T2" || { echo "  task never terminal: $STATUS"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: expected succeeded, got $STATUS"; exit 1; }
echo "  uncapped twin succeeded"

echo ">> task-budget E2E OK (ceiling kills, twin survives)"
