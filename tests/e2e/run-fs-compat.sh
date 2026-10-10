#!/usr/bin/env bash
# Filesystem-compat E2E (process-based, no Docker): the whole stack (CP SQLite
# DB + WAL, artifact root, node outbox, workspaces, repo mirror) lives on one
# filesystem and runs a mock happy path plus an online backup round-trip.
#
# Plan 6.13: Tier-1 filesystems are ext4 and xfs. CI runners only have ext4,
# so xfs cannot be covered there; this script makes the coverage runnable on
# demand wherever an xfs mount exists:
#   AG_FS_MOUNT=/mnt/xfs bash tests/e2e/run-fs-compat.sh
# Without AG_FS_MOUNT it falls back to a fresh dir under /tmp (ext4 on CI) —
# that run guards harness rot, not the filesystem. A passing xfs run proves
# SQLite WAL + fdatasync + worktree git ops behave on xfs; the storage layer
# uses no filesystem-specific ioctls (plain write_all + fdatasync), so no
# code fork per filesystem is expected.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7814}"
PORT="${AGENTGRID_PORT:-7814}"
USER="admin"
PASS="changeme"
source "$ROOT/tests/e2e/lib-bootstrap.sh"

if [ -n "${AG_FS_MOUNT:-}" ]; then
  [ -d "$AG_FS_MOUNT" ] || { echo "AG_FS_MOUNT=$AG_FS_MOUNT is not a directory"; exit 1; }
  [ -w "$AG_FS_MOUNT" ] || { echo "AG_FS_MOUNT=$AG_FS_MOUNT is not writable"; exit 1; }
  FSROOT="$AG_FS_MOUNT"
  FSROOT_CREATED=0
else
  FSROOT="$(mktemp -d -t ag-e2e-fscompat-root-XXXXXX)"
  FSROOT_CREATED=1
fi
FSTYPE="$(stat -f -c %T "$FSROOT" 2>/dev/null || echo unknown)"
echo ">> fs-compat on $FSROOT (fstype: $FSTYPE)"

TMP="$FSROOT/ag-e2e-fscompat-$$"
CP_DB="$TMP/cp.db"
NODE_DATA="$TMP/node"
WORK="$TMP/work"
REPOS="$TMP/repos"
ART="$TMP/artifacts"
mkdir -p "$NODE_DATA" "$WORK" "$REPOS" "$ART"

CP_PID=""
NODE_PID=""

cleanup() {
  set +e
  [ -n "$NODE_PID" ] && kill -9 "$NODE_PID" 2>/dev/null
  [ -n "$CP_PID" ] && kill "$CP_PID" 2>/dev/null
  pkill -f "$BIN/agentgrid-control-plane" 2>/dev/null
  pkill -f "$BIN/agentgrid-node-daemon" 2>/dev/null
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
  [ "${FSROOT_CREATED:-0}" = "1" ] && [ "${AG_E2E_KEEP:-0}" != "1" ] && rm -rf "$FSROOT"
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
  AGENTGRID_JWT_SECRET="e2e-fscompat-secret" \
  AGENTGRID_ARTIFACT_ROOT="$ART" \
  nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
  CP_PID=$!
}

start_node() {
  env PATH="$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE" \
    AGENTGRID_DATA_DIR="$NODE_DATA" \
    AGENTGRID_NODE_NAME="e2e-fscompat" \
    AGENTGRID_WORKSPACE_ROOT="$WORK" \
    AGENTGRID_REPOSITORY_ROOT="$REPOS" \
    AGENTGRID_ADAPTERS="mock" \
    AGENTGRID_MAX_CONCURRENCY="1" \
    RUST_LOG="info" \
    AGENTGRID_ENROLL_TOKEN="$ENROLL_TOKEN" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
  NODE_PID=$!
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

echo ">> starting CP with DB $CP_DB"
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
login
mint_token
start_node
wait_node_online || exit 1

echo ">> submitting mock task (spam:200 + write) on $FSTYPE"
TID=$(submit $'spam:200\nwrite:hello.txt:fs-compat-ok')
echo "  task $TID; waiting for terminal status (120s)"
if wait_terminal 120 "$TID"; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: expected succeeded, got $STATUS"; cat "$TMP/node.log"; exit 1; }

echo ">> asserting event stream is contiguous (no gap/dup on $FSTYPE)"
EVS=$(curl -fsS "$BASE/v1/tasks/$TID/events?after_sequence=0" -H "authorization: Bearer $jwt")
SEQ_OK=$(python3 -c '
import json,sys
evs = json.load(sys.stdin)
seqs = [e["sequence"] for e in evs]
ok = len(seqs) > 0 and seqs == sorted(seqs) and len(set(seqs)) == len(seqs) and seqs == list(range(seqs[0], seqs[0]+len(seqs)))
print("ok" if ok else "bad")
' <<<"$EVS")
[ "$SEQ_OK" = "ok" ] || { echo "  FAILED: event stream not contiguous"; exit 1; }
echo "  events contiguous"

echo ">> online backup round-trip on $FSTYPE (VACUUM INTO exercises SQLite-on-fs)"
# The endpoint only accepts a plain file name; the backup lands in the data
# dir (parent of the artifact root), which is on the target fs by construction.
curl -fsS -X POST "$BASE/v1/admin/backup" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d '{"path":"fscompat-backup.db"}' >/dev/null || { echo "  backup endpoint failed"; cat "$TMP/cp.log"; exit 1; }
BKP="$TMP/fscompat-backup.db"
[ -s "$BKP" ] || { echo "  FAILED: backup file missing/empty at $BKP"; exit 1; }
echo "  backup ok ($(stat -c %s "$BKP") bytes)"

echo ">> fs-compat OK on fstype $FSTYPE (happy path + contiguous events + backup)"
