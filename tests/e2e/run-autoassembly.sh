#!/usr/bin/env bash
# E2E (process-based, no Docker, no GitHub, no LLM): the one-button loop.
#
# POST /v1/assembly with a goal → ephemeral architect template → plan-stub
# architect emits the worker plan → auto-approve expands (no human) →
# workers on two nodes write their parts → system-appended collector merges
# both into one commit. Asserts the run succeeds and the collector patch
# carries both parts.
#
# The stub speaks the wrapper plan-event contract, so plan capture,
# PlanReady/auto-approve and collector-append are all exercised exactly as
# with a real planning agent (which would take `adapter: <real>` instead).
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7827}"
PORT="${AGENTGRID_PORT:-7827}"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-autoasm-secret"
E2E_CONCURRENCY=2

TMP="$(mktemp -d -t ag-e2e-autoasm-XXXXXX)"
mkdir -p "$TMP/n1" "$TMP/w1" "$TMP/r1" "$TMP/n2" "$TMP/w2" "$TMP/r2" "$TMP/stubbin"
cp "$ROOT/tests/e2e/plan-stub.sh" "$TMP/stubbin/adapter-plan-stub"
chmod +x "$TMP/stubbin/adapter-plan-stub"

# Shared local-path repo both nodes clone (file path = offline "remote").
git init -q -b main "$TMP/origin"
git -C "$TMP/origin" -c user.name=t -c user.email=t@x commit -q --allow-empty -m init

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

echo ">> one-button assembly E2E (plan-stub architect, no LLM)"
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node n1 "e2e-autoasm-n1" "mock,plan-stub" "$TMP/w1" "$TMP/r1" "$TMP/n1" "PATH=$TMP/stubbin:$BIN:$PATH"
# Enrollment tokens are one-shot: n1 consumed the minted token, so n2 needs
# a freshly minted one (same pattern as run-cp-restart.sh).
e2e_mint_token
e2e_start_node n2 "e2e-autoasm-n2" "mock" "$TMP/w2" "$TMP/r2" "$TMP/n2"
e2e_wait_nodes_online 2 || exit 1

echo ">> registering the shared local repo"
curl -fsS -X POST "$BASE/v1/repositories" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d "{\"name\":\"asm\",\"git_url\":\"$TMP/origin\",\"default_branch\":\"main\"}" >/dev/null \
  || { echo "repo register failed"; exit 1; }

echo ">> POST /v1/assembly with a goal (one call, no template)"
RID=$(curl -fsS -X POST "$BASE/v1/assembly" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d '{"goal":"assemble a two-part demo","repository":"asm","adapter":"plan-stub","auto_approve_plan":true}' \
  | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d["id"])')
echo "  run $RID"
curl -fsS "$BASE/v1/workflow-runs/$RID" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;d=json.load(sys.stdin);r=d["run"];assert r["assembly"] is True,"not marked assembly";assert r["auto_approve_plan"] is True;print("  assembly run, auto-approved")'

echo ">> ticking to terminal (no human approval)"
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
[ "$status" = "succeeded" ] || { echo "  FAILED: run $RID -> $status"; exit 1; }
echo "  run succeeded without human approval"

echo ">> collector merged both parts (system-appended step)"
CTID=$(curl -fsS "$BASE/v1/workflow-runs/$RID/projection" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;ss=json.load(sys.stdin)["steps"];print([s["task_id"] for s in ss if s["step_id"]=="collect"][0])')
[ -n "$CTID" ] && [ "$CTID" != "None" ] || { echo "  FAILED: no system collector step"; exit 1; }
curl -fsS "$BASE/v1/tasks/$CTID/artifacts/changes.patch" -H "authorization: Bearer $jwt" -o "$TMP/collector.patch" \
  || { echo "  FAILED: no collector patch artifact"; exit 1; }
grep -q "part-a.txt" "$TMP/collector.patch" || { echo "  FAILED: part-a missing"; exit 1; }
grep -q "part-b.txt" "$TMP/collector.patch" || { echo "  FAILED: part-b missing"; exit 1; }
echo "  collector patch carries both parts"

echo ">> one-button assembly E2E OK (goal in, merged project out)"
