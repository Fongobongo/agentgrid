#!/usr/bin/env bash
# E2E (process-based, no Docker): node disk-full fail-closed via outbox spool
# limit.
#
# Plan item: «Переполнение диска на node (spool limit)» (Stage 2.x failure
# injection). When the control plane is unreachable for a long window and the
# node keeps streaming adapter output, the on-disk event outbox would grow
# without bound and fill the host disk. The outbox enforces
# `AGENTGRID_OUTBOX_SPOOL_LIMIT_MB`; once exceeded, `push` returns `SpoolFull`,
# the sink latches `spool_full`, emits a terminal `error` event, and the
# attempt completes with `error_code=spool_full` (fail-closed: no more events
# are buffered, the disk is protected).
#
# This script drives that path end-to-end: hold the CP down while a chatty mock
# task runs against a tiny spool limit (1 MiB); bring the CP back; assert the
# task reached `failed` with `error_code=spool_full` in its events and the
# outbox file never exceeded ~1 MiB.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7813}"
PORT="${AGENTGRID_PORT:-7813}"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-stable-secret"
E2E_CONCURRENCY=1
SPOOL_BYTES="${AGENTGRID_OUTBOX_SPOOL_LIMIT_BYTES:-4096}"

TMP="$(mktemp -d -t ag-e2e-diskfull-XXXXXX)"
NODE_DATA="$TMP/node"
WORK="$TMP/work"
REPOS="$TMP/repos"
mkdir -p "$NODE_DATA" "$WORK" "$REPOS"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

echo ">> node disk-full fail-closed (spool limit ${SPOOL_BYTES} bytes)"
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node node "e2e-disk" "mock" "$WORK" "$REPOS" "$NODE_DATA" \
  "AGENTGRID_OUTBOX_SPOOL_LIMIT_BYTES=$SPOOL_BYTES"
e2e_wait_nodes_online 1 || exit 1

echo ">> submitting chatty task (spam:5000 events → ~150 KiB > 4 KiB limit) while CP is up"
TID=$(e2e_submit "spam:5000" "mock" "*" 60)
echo "  task $TID; waiting 1s for assignment"
sleep 1

echo ">> stopping CP so the node can't flush → outbox grows toward the limit"
e2e_stop_cp
echo "  CP down; node streams into the outbox; waiting 8s for spool_full latch (4 KiB limit ≈ 80 lines)"
sleep 8

echo ">> restarting CP (node redelivers the spool_full completion)"
e2e_start_cp
e2e_wait_ready || { echo "CP not ready after restart"; cat "$TMP/cp.log"; exit 1; }
e2e_login

echo ">> polling task for terminal status (expect failed; timeout 30s)"
if e2e_wait_terminal 30 "$TID"; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "failed" ] || { echo "  FAILED: expected failed (spool_full), got $STATUS"; cat "$TMP/node.log"; exit 1; }

echo ">> asserting a spool_full error event was emitted"
EVS=$(e2e_task_events "$TID")
python3 <<PYEOF
import json
evs = json.loads('''$EVS''') if '''$EVS'''.strip() else []
spool = [e for e in evs if e.get("payload",{}).get("event") == "spool_full" or e.get("payload",{}).get("error_code") == "spool_full"]
if not spool:
    print("  FAILED: no spool_full event found"); raise SystemExit(1)
print("  spool_full event present (seq={})".format(spool[0]["sequence"]))
PYEOF

echo ">> asserting the outbox file never grew far past the ${SPOOL_BYTES}-byte limit"
# The file may overshoot by one event line, but must stay bounded (well under
# 2x the limit). Find the attempt's outbox file.
OB_FILE=$(find "$NODE_DATA/outbox" -name '*.jsonl' ! -name 'completions.jsonl' -printf '%p %s\n' 2>/dev/null | sort -k2 -rn | head -1 | cut -d' ' -f1)
if [ -z "$OB_FILE" ]; then
  echo "  (no attempt outbox file found — completion was acked and trimmed; ok)"
else
  SIZE=$(stat -c %s "$OB_FILE")
  CEILING=$((SPOOL_BYTES * 2))
  echo "  outbox file $OB_FILE = ${SIZE} bytes (limit ${SPOOL_BYTES}, ceiling ${CEILING})"
  [ "$SIZE" -le "$CEILING" ] || { echo "  FAILED: outbox exceeded 2x limit"; exit 1; }
fi

echo ">> disk-full fail-closed OK: attempt failed with spool_full, disk bounded"
