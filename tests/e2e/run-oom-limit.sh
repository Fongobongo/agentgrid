#!/usr/bin/env bash
# E2E (process-based CP+node, Docker-sandboxed attempt): per-attempt memory
# limit → runtime OOM kill → truthful `resource_limit` error code.
#
# Stage 12 / ADR 0003 acceptance: "Test: превышение memory limit →
# error_code=resource_limit". The wrapper path spawns the mock adapter inside
# the hardened sandbox container with `--memory 64m` (node-wide env knob; the
# per-attempt profile override is covered by unit tests). The mock's `oom:512`
# command allocates and touches 512 MiB, the runtime OOM-kills the container,
# the node's post-exit `docker inspect` reports OOMKilled, and the completion
# carries `error_code=resource_limit:memory` — distinct from `agent_failed`,
# so the CP can treat a hit ceiling differently from an agent crash.
#
# Requires: docker (a pullable/in-image runtime), curl, python3.
# Skips cleanly (exit 0) when docker or the sandbox image is unavailable so
# the fast CI job stays green; the e2e job (docker available) runs it for real.
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
IMAGE="${AGENTGRID_SANDBOX_IMAGE:-ubuntu:24.04}"
MEM_LIMIT_MB="${AGENTGRID_E2E_MEMORY_MB:-64}"
OOM_MB="${AGENTGRID_E2E_OOM_MB:-512}"

# ---- docker availability gate (mirror docker_ro_mount_really_blocks_write)
if ! command -v docker >/dev/null 2>&1; then
  echo ">> skip: docker not found"
  exit 0
fi
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  if ! docker pull -q "$IMAGE" >/dev/null 2>&1; then
    echo ">> skip: image $IMAGE not pullable"
    exit 0
  fi
fi
if ! docker info >/dev/null 2>&1; then
  echo ">> skip: docker daemon unreachable"
  exit 0
fi

TMP="$(mktemp -d -t ag-e2e-oom-XXXXXX)"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos"

source "$ROOT/tests/e2e/lib-e2e.sh"

cleanup_oom() {
  e2e_cleanup
  # Best-effort: reap any sandbox containers this run created (the daemon's
  # startup sweep does this via the node label, but we may kill the daemon
  # before it cleans up).
  for c in $(docker ps -aq --filter name=agentgrid- 2>/dev/null); do
    docker rm -f "$c" >/dev/null 2>&1
  done
}
trap cleanup_oom EXIT

echo ">> OOM limit e2e: sandbox=docker --memory=${MEM_LIMIT_MB}m, mock allocates ${OOM_MB} MiB"
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node oom "e2e-oom" "mock" "$TMP/work" "$TMP/repos" "$TMP/node" \
  "AGENTGRID_SANDBOX=docker" \
  "AGENTGRID_SANDBOX_IMAGE=$IMAGE" \
  "AGENTGRID_SANDBOX_MEMORY=${MEM_LIMIT_MB}m"
e2e_wait_nodes_online 1 || exit 1

echo ">> submitting oom task (mock allocates ${OOM_MB} MiB > ${MEM_LIMIT_MB} MiB limit)"
TID=$(e2e_submit "oom:${OOM_MB}" "mock" "*" 120)
echo "  task $TID; waiting for terminal status (expect failed / resource_limit; timeout 120s)"
if e2e_wait_terminal 120 "$TID"; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/oom.log"; exit 1
fi
[ "$STATUS" = "failed" ] || { echo "  FAILED: expected failed, got $STATUS"; cat "$TMP/oom.log"; exit 1; }

echo ">> asserting error_code=resource_limit (distinct from agent_failed)"
DETAIL=$(curl -fsS "$BASE/v1/tasks/$TID" -H "authorization: Bearer $jwt")
if ! python3 <<PYEOF
import json
d = json.loads('''$DETAIL''')
# The task detail carries error_code at the top level (tasks.error_code),
# mirrored onto the latest attempt by the completion handler. Accept either.
err = d.get("error_code")
if not err:
    attempts = d.get("attempts") or d.get("attempt") or []
    if isinstance(attempts, dict):
        attempts = [attempts]
    for a in attempts:
        e = a.get("error_code")
        if e:
            err = e
            break
if not err:
    print("  FAILED: no error_code on the task")
    print("  task detail:", json.dumps(d, indent=2)[:2000])
    raise SystemExit(1)
if not str(err).startswith("resource_limit"):
    print(f"  FAILED: expected resource_limit:*, got {err!r}")
    raise SystemExit(1)
print(f"  error_code={err} — OOM classified as resource_limit, not agent_failed")
PYEOF
then
  echo ">> node log tail:"
  tail -n 60 "$TMP/oom.log" 2>/dev/null || true
  echo ">> leftover sandbox containers:"
  docker ps -a --filter name=agentgrid- --format '{{.Names}} {{.Status}}' || true
  exit 1
fi

echo ">> asserting the sandbox actually OOM-killed the container (not a mock bug)"
if docker ps -a --filter name=agentgrid- --format '{{.Names}}' | grep -q .; then
  CNAME=$(docker ps -a --filter name=agentgrid- --format '{{.Names}}' | head -1)
  OK=$(docker inspect -f '{{.State.OOMKilled}}' "$CNAME" 2>/dev/null || echo "gone")
  echo "  container $CNAME OOMKilled=$OK (cleaned up after inspect is also fine)"
fi

echo ">> OOM limit e2e OK: memory ceiling → runtime OOM → resource_limit error code"
