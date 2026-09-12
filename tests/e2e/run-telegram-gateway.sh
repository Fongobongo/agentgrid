#!/usr/bin/env bash
# Telegram gateway E2E against a mock Bot API (no Docker, no real Telegram).
#
# Scenario: CP + mock Bot API (AGENTGRID_TELEGRAM_API override) +
# agentgrid-gateway. The mock feeds a /approvals command and an inline-keyboard
# callback (ag:deny:<id>) to the gateway as Telegram updates; the script
# asserts:
#   1. the /approvals reply contains the pending permission AND an inline
#      keyboard whose buttons carry the approval id,
#   2. the deny callback tap answers the approval CP-side (status=denied),
#   3. /nodes works through the same pipe (dispatch smoke).
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

PORT="${AGENTGRID_PORT:-7821}"
BASE="http://127.0.0.1:$PORT"
TG_PORT=18901
TG_URL="http://127.0.0.1:$TG_PORT"
TOKEN="e2e-bot-token"
USER="admin"
PASS="changeme"
CHAT_ID=4242
source "$ROOT/tests/e2e/lib-bootstrap.sh"

TMP="$(mktemp -d -t ag-e2e-tg-XXXXXX)"
PIDS=()
cleanup() {
  set +e
  for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

# 1. Control plane.
AGENTGRID_LISTEN="127.0.0.1:$PORT" \
AGENTGRID_DB="$TMP/cp.db" \
AGENTGRID_JWT_SECRET="e2e-tg-secret" \
AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
PIDS+=($!)

for _ in $(seq 1 40); do
  curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fsS "$BASE/health/ready" >/dev/null || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"

jwt=$(curl -fsS -X POST "$BASE/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

# 2. Mock Bot API (records sendMessage/answerCallbackQuery bodies).
python3 "$ROOT/tests/e2e/mini-telegram-bot.py" "$TG_PORT" "$TMP/hits.jsonl" & PIDS+=($!)
sleep 0.5

# 3. Create a pending approval directly (task -> approval row via the API).
task_id=$(curl -fsS -X POST "$BASE/v1/tasks" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d '{"prompt":"tg e2e","repository":"demo","adapter":"mock"}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
approval_id=$(curl -fsS -X POST "$BASE/v1/tasks/$task_id/approvals" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d '{"attempt_id":"e2e-attempt","permission":"bash","scope":"repo:demo"}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
echo "  created task $task_id approval $approval_id"

# 4. Gateway pointed at the mock via the API-base env override.
AGENTGRID_GATEWAY_ADMINS="$CHAT_ID" \
AGENTGRID_TELEGRAM_API="$TG_URL" \
nohup "$BIN/agentgrid-gateway" run \
  --control-plane "$BASE" \
  --token "$jwt" \
  --telegram "$TOKEN" \
  >"$TMP/gw.log" 2>&1 &
PIDS+=($!)
sleep 1

# 5. Feed the gateway: /nodes, /approvals, then a deny callback tap.
# The mock exposes an /enqueue control endpoint (see mini-telegram-bot.py).
enqueue() {
  python3 - "$1" "$2" "$CHAT_ID" "$TG_PORT" <<'PY'
import json, sys, time, urllib.request
kind, arg, chat, port = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
if kind == "cmd":
    upd = {"update_id": int(time.time() * 1000) % 10**9, "message": {"chat": {"id": chat}, "text": arg}}
elif kind == "cb":
    upd = {"update_id": int(time.time() * 1000) % 10**9, "callback_query": {
        "id": "cbq1", "data": arg,
        "message": {"chat": {"id": chat}, "text": "tap"},
        "from": {"id": chat}}}
urllib.request.urlopen(f"http://127.0.0.1:{port}/enqueue", data=json.dumps(upd).encode())
PY
}
enqueue cmd "/nodes"
sleep 2
python3 - "$TMP/hits.jsonl" <<'PY' || { echo "FAIL: /nodes reply not recorded"; cat "$TMP/hits.jsonl" "$TMP/gw.log"; exit 1; }
import json, sys
hits = [json.loads(l) for l in open(sys.argv[1], encoding="utf-8")]
texts = [h.get("text") or "" for h in hits if h.get("kind") == "send"]
assert any("online" in t or "no nodes" in t for t in texts), f"/nodes reply missing: {texts}"
print("  /nodes reply delivered")
PY

enqueue cmd "/approvals"
sleep 2
python3 - "$TMP/hits.jsonl" "$approval_id" <<'PY' || { cat "$TMP/hits.jsonl" "$TMP/gw.log"; exit 1; }
import json, sys
hits = [json.loads(l) for l in open(sys.argv[1], encoding="utf-8")]
aid = sys.argv[2]
texts = [h for h in hits if h.get("kind") == "send"]
assert any("bash" in (h.get("text") or "") for h in texts), "approval text missing in replies"
kb = [h.get("reply_markup") for h in texts if h.get("reply_markup")]
assert kb, "no inline keyboard on /approvals reply"
flat = json.dumps(kb)
assert f"ag:allow:{aid}" in flat and f"ag:deny:{aid}" in flat, f"approval id missing on buttons: {aid}"
print("  /approvals reply carries permission + keyboard")
PY

enqueue cb "ag:deny:$approval_id"
sleep 2
status=$(curl -fsS "$BASE/v1/approvals/$approval_id" \
  -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
[ "$status" = "denied" ] || { echo "FAIL: approval status=$status (expected denied)"; cat "$TMP/gw.log"; exit 1; }
grep -q '"kind": "ack"' "$TMP/hits.jsonl" || { echo "FAIL: callback not acked"; cat "$TMP/hits.jsonl"; exit 1; }

echo "OK: telegram gateway e2e (approvals listing + keyboard + deny callback)"
