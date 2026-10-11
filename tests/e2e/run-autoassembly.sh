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

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7821}"
PORT="${AGENTGRID_PORT:-7821}"
USER="admin"
PASS="changeme"
source "$ROOT/tests/e2e/lib-bootstrap.sh"

TMP="$(mktemp -d -t ag-e2e-autoasm-XXXXXX)"
CP_DB="$TMP/cp.db"
mkdir -p "$TMP/n1" "$TMP/w1" "$TMP/r1" "$TMP/n2" "$TMP/w2" "$TMP/r2" "$TMP/stubbin"
cp "$ROOT/tests/e2e/plan-stub.sh" "$TMP/stubbin/adapter-plan-stub"
chmod +x "$TMP/stubbin/adapter-plan-stub"

# Shared local-path repo both nodes clone (file path = offline "remote").
git init -q -b main "$TMP/origin"
git -C "$TMP/origin" -c user.name=t -c user.email=t@x commit -q --allow-empty -m init

CP_PID=""
N1_PID=""
N2_PID=""

cleanup() {
  set +e
  [ -n "$N2_PID" ] && kill -9 "$N2_PID" 2>/dev/null
  [ -n "$N1_PID" ] && kill -9 "$N1_PID" 2>/dev/null
  [ -n "$CP_PID" ] && kill "$CP_PID" 2>/dev/null
  pkill -f "$BIN/agentgrid-control-plane" 2>/dev/null
  pkill -f "$BIN/agentgrid-node-daemon" 2>/dev/null
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

wait_ready() {
  for _ in $(seq 1 40); do
    curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}

login() {
  jwt=$(curl -fsS -X POST "$BASE/v1/auth/login" \
    -H 'content-type: application/json' \
    -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  [ -n "$jwt" ] || { echo "login failed"; cat "$TMP/cp.log"; exit 1; }
}

mint_token() {
  ENROLL_TOKEN=$(curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
    -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  [ -n "$ENROLL_TOKEN" ] || { echo "mint token failed"; exit 1; }
}

start_cp() {
  AGENTGRID_LISTEN="127.0.0.1:$PORT" \
  AGENTGRID_DB="$CP_DB" \
  AGENTGRID_JWT_SECRET="e2e-autoasm-secret" \
  AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
  nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
  CP_PID=$!
}

# $1 = tag (n1/n2), $2 = adapters csv
start_node() {
  local n="$1" idx=1
  [ "$n" = "n2" ] && idx=2
  env PATH="$TMP/stubbin:$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE" \
    AGENTGRID_DATA_DIR="$TMP/$n" \
    AGENTGRID_NODE_NAME="e2e-autoasm-$n" \
    AGENTGRID_WORKSPACE_ROOT="$TMP/w$idx" \
    AGENTGRID_REPOSITORY_ROOT="$TMP/r$idx" \
    AGENTGRID_ADAPTERS="$2" \
    AGENTGRID_MAX_CONCURRENCY="2" \
    RUST_LOG="info" \
    AGENTGRID_ENROLL_TOKEN="$ENROLL_TOKEN" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/$n.log" 2>&1 &
  if [ "$n" = "n1" ]; then N1_PID=$!; else N2_PID=$!; fi
}

wait_nodes_online() {
  for _ in $(seq 1 60); do
    n=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" 2>/dev/null \
      | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(sum(1 for x in ns if x.get("status")=="online"))' 2>/dev/null) || n=0
    [ "$n" = "2" ] && return 0
    sleep 0.5
  done
  echo "wanted 2 online nodes, have $n"; return 1
}

echo ">> one-button assembly E2E (plan-stub architect, no LLM)"
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
login
mint_token
start_node n1 "mock,plan-stub"
start_node n2 "mock"
wait_nodes_online || exit 1

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
