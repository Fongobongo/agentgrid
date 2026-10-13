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
PASS="changeme"
E2E_JWT_SECRET="e2e-stable-secret"
E2E_CONCURRENCY=1
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

# Enforcement canary (two steps): a reachable user manager does NOT imply
# working scopes or enforceable limits (current GH runners answer
# is-system-running but lack cgroup delegation into the user slice, so scope
# creation misbehaves and MemoryMax is silently unenforced — scenario A would
# then fail testing the HOST, not our code).
#  1. A trivial scope must start at all.
#  2. A 32M scope must OOM-kill a 128M allocation (rc!=0,quick); a clean
#     exit (rc==0) means the allocation survived → limits unenforced → skip.
echo ">> canary 1/2: trivial transient scope must start"
timeout 30 systemd-run --user --scope -q --wait true 2>/dev/null \
  || { echo ">> skip: transient scopes do not start on this host (no delegation?) — need a real systemd host"; exit 0; }
echo ">> canary 2/2: 128M alloc in a 32M scope must die"
if timeout 60 systemd-run --user --scope -q -p MemoryMax=32M --wait \
  python3 -c "b = bytearray(128 * 1024 * 1024); import time; time.sleep(5)" 2>/dev/null; then
  echo ">> skip: MemoryMax unenforced on this host (128M survived a 32M scope) — need cgroup delegation"
  exit 0
else
  rc=$?
  if [ "$rc" -eq 124 ]; then
    echo ">> skip: scope canary timed out (user manager wedged?)"
    exit 0
  fi
  echo ">> canary OOM-killed as expected (rc=$rc) — limits are enforced"
fi

# The mock adapter + `sleep` must resolve; the wrapper path spawns
# `adapter-mock` from PATH.
[ -x "$BIN/adapter-mock" ] || { echo ">> skip: adapter-mock not built (cargo build -p agentgrid-adapters)"; exit 0; }
command -v sleep >/dev/null 2>&1 || { echo ">> skip: sleep(1) missing"; exit 0; }

TMP="$(mktemp -d -t ag-e2e-scope-XXXXXX)"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos"

source "$ROOT/tests/e2e/lib-e2e.sh"

cleanup_scope() {
  e2e_cleanup
  # Best-effort: stop any scopes this run left in the user manager (the
  # daemon's startup sweep does this too, but we may kill it first).
  for u in $(systemctl --user list-units --all --plain --no-legend 2>/dev/null | awk '$1 ~ /^agentgrid-scope-/ {print $1}'); do
    systemctl --user stop "$u" >/dev/null 2>&1
  done
}
trap cleanup_scope EXIT

error_code_of() {
  curl -fsS "$BASE/v1/tasks/$1" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin).get("error_code") or "")'
}

echo ">> systemd scope e2e: sandbox=systemd MemoryMax=${MEM_LIMIT_MB}M TasksMax=$TASKS_MAX"
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node scope "e2e-scope" "mock" "$TMP/work" "$TMP/repos" "$TMP/node" \
  "AGENTGRID_SANDBOX=systemd" \
  "AGENTGRID_SANDBOX_MEMORY=${MEM_LIMIT_MB}M" \
  "AGENTGRID_SANDBOX_PIDS_LIMIT=$TASKS_MAX"
e2e_wait_nodes_online 1 || exit 1

echo ">> scenario A: MemoryMax=${MEM_LIMIT_MB}M, mock allocates ${OOM_MB} MiB"
TID_A=$(e2e_submit "oom:${OOM_MB}" "mock" "*" 120)
echo "  task $TID_A; waiting for terminal status (expect failed / resource_limit:memory; timeout 120s)"
if e2e_wait_terminal 120 "$TID_A"; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/scope.log"; exit 1
fi
[ "$STATUS" = "failed" ] || { echo "  FAILED: expected failed, got $STATUS"; cat "$TMP/scope.log"; exit 1; }
ERR_A=$(error_code_of "$TID_A")
case "$ERR_A" in
  resource_limit*) echo "  error_code=$ERR_A — scope OOM classified as resource_limit";;
  *) echo "  FAILED: expected resource_limit:*, got '$ERR_A'"; cat "$TMP/scope.log"; exit 1;;
esac

echo ">> scenario B: TasksMax=$TASKS_MAX, mock forks $FORKS children"
TID_B=$(e2e_submit "fork:$FORKS" "mock" "*" 120)
echo "  task $TID_B; waiting for terminal status (expect failed; timeout 120s)"
if e2e_wait_terminal 120 "$TID_B"; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/scope.log"; exit 1
fi
[ "$STATUS" = "failed" ] || { echo "  FAILED: expected failed, got $STATUS"; cat "$TMP/scope.log"; exit 1; }
echo "  error_code=$(error_code_of "$TID_B") — excess forks refused by the TasksMax ceiling"

echo ">> systemd scope e2e OK: MemoryMax → OOM → resource_limit; TasksMax → fork refusal → failed"
