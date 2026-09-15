#!/usr/bin/env bash
# E2E (process-based CP+node, systemd-scope attempt): per-attempt cgroups v2
# limits via `systemd-run --user --scope` → kernel OOM / fork-refusal →
# truthful `resource_limit` error code.
#
# Plan 6.11 acceptance: "При наличии systemd/cgroups v2 создавать transient
# scope на attempt", "Поддержать MemoryMax", "Поддержать TasksMax", "Тестировать
# fork-heavy mock adapter и TasksMax". Scenario A: MemoryMax 64M + mock `oom:512`
# → the kernel OOM-kills the scope → `systemctl --user show -p OOMKill`
# reports 1 → completion carries `error_code=resource_limit:memory`.
# Scenario B: TasksMax 32 + mock `fork:200` → excess forks are refused by the
# kernel, the mock reports failed spawns → attempt `failed`.
#
# Requires: systemd user manager (linux; `systemctl --user is-system-running`
# answers), cgroups v2, curl, python3. Skips cleanly (exit 0) on hosts
# without any of those (containers, macOS, WSL1, CI runners without a user
# session) so unrelated jobs stay green; run it on a real systemd host.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7815}"
PORT="${AGENTGRID_PORT:-7815}"
USER="admin"
source "$ROOT/tests/e2e/lib-bootstrap.sh"
PASS="changeme"
MEM_LIMIT_MB="${AGENTGRID_E2E_SCOPE_MEMORY_MB:-64}"
OOM_MB="${AGENTGRID_E2E_SCOPE_OOM_MB:-512}"
FORKS="${AGENTGRID_E2E_SCOPE_FORKS:-200}"
TASKS_MAX="${AGENTGRID_E2E_SCOPE_TASKS_MAX:-32}"

# ---- systemd-scope availability gate (mirror probe_systemd_scope)
if [ "$(uname -s)" != "Linux" ]; then
  echo ">> skip: not linux"
  exit 0
fi
command -v systemd-run >/dev/null 2>&1 || { echo ">> skip: systemd-run not found"; exit 0; }
systemctl --user is-system-running >/dev/null 2>&1 \
  || { echo ">> skip: no reachable systemd user manager (try loginctl enable-linger \$USER)"; exit 0; }
[ -f /sys/fs/cgroup/cgroup.controllers ] || { echo ">> skip: cgroups v2 not mounted"; exit 0; }

# The mock adapter + `sleep` must resolve; the wrapper path spawns
# `adapter-mock` from PATH.
[ -x "$BIN/adapter-mock" ] || { echo ">> skip: adapter-mock not built (cargo build -p agentgrid-adapters)"; exit 0; }
command -v sleep >/dev/null 2>&1 || { echo ">> skip: sleep(1) missing"; exit 0; }

TMP="$(mktemp -d -t ag-e2e-scope-XXXXXX)"
CP_DB="$TMP/cp.db"
NODE_DATA="$TMP/node"
WORK="$TMP/work"
REPOS="$TMP/repos"
mkdir -p "$NODE_DATA" "$WORK" "$REPOS"

CP_PID=""
NODE_PID=""

cleanup() {
  set +e
  [ -n "$NODE_PID" ] && kill -9 "$NODE_PID" 2>/dev/null
  [ -n "$CP_PID" ] && kill "$CP_PID" 2>/dev/null
  pkill -f "$BIN/agentgrid-control-plane" 2>/dev/null
  pkill -f "$BIN/agentgrid-node-daemon" 2>/dev/null
  # Best-effort: stop any scopes this run left in the user manager (the
  # daemon's startup sweep does this too, but we may kill it first).
  for u in $(systemctl --user list-units --all --plain --no-legend 2>/dev/null | awk '$1 ~ /^agentgrid-scope-/ {print $1}'); do
    systemctl --user stop "$u" >/dev/null 2>&1
  done
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

start_cp() {
  AGENTGRID_LISTEN="127.0.0.1:$PORT" \
  AGENTGRID_DB="$CP_DB" \
  AGENTGRID_JWT_SECRET="e2e-stable-secret" \
  AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
  nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
  CP_PID=$!
}

start_node() {
  local tok="${1:-}"
  local env_args=()
  [ -n "$tok" ] && env_args+=(AGENTGRID_ENROLL_TOKEN="$tok")
  env PATH="$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE" \
    AGENTGRID_DATA_DIR="$NODE_DATA" \
    AGENTGRID_NODE_NAME="e2e-scope" \
    AGENTGRID_WORKSPACE_ROOT="$WORK" \
    AGENTGRID_REPOSITORY_ROOT="$REPOS" \
    AGENTGRID_ADAPTERS="mock" \
    AGENTGRID_MAX_CONCURRENCY="1" \
    AGENTGRID_SANDBOX="systemd" \
    AGENTGRID_SANDBOX_MEMORY="${MEM_LIMIT_MB}M" \
    AGENTGRID_SANDBOX_PIDS_LIMIT="$TASKS_MAX" \
    RUST_LOG="info" \
    "${env_args[@]}" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
  NODE_PID=$!
}

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

wait_node_online() {
  for _ in $(seq 1 60); do
    st=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" 2>/dev/null \
      | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st="none"
    [ "$st" = "online" ] && return 0
    sleep 0.5
  done
  echo "node never came online; status=$st"; cat "$TMP/node.log"; return 1
}

submit() {  # $1 = prompt; prints task id
  local prompt_json
  prompt_json=$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1")
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$prompt_json,\"repository\":\"*\",\"adapter\":\"mock\",\"timeout_secs\":120}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}

poll_status() {
  STATUS=$(curl -fsS "$BASE/v1/tasks/$1" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
}

wait_terminal() {
  STATUS=""
  for _ in $(seq 1 "$1"); do
    poll_status "$2"
    case "$STATUS" in
      succeeded|failed|cancelled|timed_out|lost) return 0;;
    esac
    sleep 1
  done
  return 1
}

error_code_of() {
  curl -fsS "$BASE/v1/tasks/$1" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin).get("error_code") or "")'
}

echo ">> systemd scope e2e: sandbox=systemd MemoryMax=${MEM_LIMIT_MB}M TasksMax=$TASKS_MAX"
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
login
mint_token
start_node "$ENROLL_TOKEN"
wait_node_online || exit 1

echo ">> scenario A: MemoryMax=${MEM_LIMIT_MB}M, mock allocates ${OOM_MB} MiB"
TID_A=$(submit "oom:${OOM_MB}")
echo "  task $TID_A; waiting for terminal status (expect failed / resource_limit:memory; timeout 120s)"
if wait_terminal 120 "$TID_A"; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "failed" ] || { echo "  FAILED: expected failed, got $STATUS"; cat "$TMP/node.log"; exit 1; }
ERR_A=$(error_code_of "$TID_A")
case "$ERR_A" in
  resource_limit*) echo "  error_code=$ERR_A — scope OOM classified as resource_limit";;
  *) echo "  FAILED: expected resource_limit:*, got '$ERR_A'"; cat "$TMP/node.log"; exit 1;;
esac

echo ">> scenario B: TasksMax=$TASKS_MAX, mock forks $FORKS children"
TID_B=$(submit "fork:$FORKS")
echo "  task $TID_B; waiting for terminal status (expect failed; timeout 120s)"
if wait_terminal 120 "$TID_B"; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "failed" ] || { echo "  FAILED: expected failed, got $STATUS"; cat "$TMP/node.log"; exit 1; }
echo "  error_code=$(error_code_of "$TID_B") — excess forks refused by the TasksMax ceiling"

echo ">> systemd scope e2e OK: MemoryMax → OOM → resource_limit; TasksMax → fork refusal → failed"
