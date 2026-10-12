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

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7824}"
PORT="${AGENTGRID_PORT:-7824}"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-fscompat-secret"
E2E_CONCURRENCY=1

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

source "$ROOT/tests/e2e/lib-e2e.sh"

cleanup_fscompat() {
  e2e_cleanup
  [ "${FSROOT_CREATED:-0}" = "1" ] && [ "${AG_E2E_KEEP:-0}" != "1" ] && rm -rf "$FSROOT"
}
trap cleanup_fscompat EXIT

echo ">> starting CP with DB $CP_DB"
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
# Artifact root must be the fs-local one (lib defaults to $TMP/artifacts,
# which already is — ART==$TMP/artifacts by construction here).
e2e_start_node fscompat "e2e-fscompat" "mock" "$WORK" "$REPOS" "$NODE_DATA"
e2e_wait_nodes_online 1 || exit 1

echo ">> submitting mock task (spam:200 + write) on $FSTYPE"
TID=$(e2e_submit $'spam:200\nwrite:hello.txt:fs-compat-ok' "mock" "*" 120)
echo "  task $TID; waiting for terminal status (120s)"
e2e_wait_terminal 120 "$TID" || { echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/fscompat.log"; exit 1; }
echo "  final status: $STATUS"
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: expected succeeded, got $STATUS"; cat "$TMP/fscompat.log"; exit 1; }

echo ">> asserting event stream is contiguous (no gap/dup on $FSTYPE)"
EVS=$(e2e_task_events "$TID")
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
