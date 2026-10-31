#!/usr/bin/env bash
# E2E (process-based, no Docker, no LLM): sealed adapter-env secrets.
#
# ADR 0028 phase 1B. The unit tests prove the seal round-trips and that the
# store keeps ciphertext; this proves the wiring, on a real control plane and a
# real node, in the order the pieces can break:
#
#   1. a value set through `ag adapter-env set` **from stdin** is stored, and
#      the read path returns a fingerprint and a length — never the value;
#   2. the control plane's own files never hold the plaintext (`cp.db`,
#      `cp.db-wal`, `cp.log`), even though it just handled it in memory;
#   3. the node opens the blob and the adapter process really receives the
#      variable: the mock's `env:<VAR>` command prints what the child saw;
#   4. a secret-named variable is masked in the event stream and a non-secret
#      one is not — so (3) cannot pass by redacting everything, and the raw
#      secret never reaches the event log;
#   5. `ag adapter-env rm` revokes the value on a *running* node (the next
#      attempt sees it unset), which is the only revocation verb the API has.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="${AGENTGRID_BIN:-$ROOT/target/debug}"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7851}"
PORT="${AGENTGRID_PORT:-7851}"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-sealed-env-secret"
E2E_CONCURRENCY=2

# Distinctive enough to grep for in binary files without false positives.
SECRET="sk-ant-e2e-SUPERSECRET-4242"
BASEURL="https://sealed.example/v1"

TMP="$(mktemp -d -t ag-e2e-sealed-XXXXXX)"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos" "$TMP/cfg"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

# `ag` keeps its token under the config dir, so redirect it into the sandbox.
export XDG_CONFIG_HOME="$TMP/cfg"
# A corporate proxy on the host would swallow the loopback calls the CLI makes.
unset HTTP_PROXY HTTPS_PROXY http_proxy https_proxy ALL_PROXY all_proxy 2>/dev/null || true
export NO_PROXY="127.0.0.1,localhost"

# `e2e_check_bins` predates the CLI and only looks for `ag`, which is `ag.exe`
# under git bash.
AG_BIN=""
for cand in "$BIN/ag" "$BIN/ag.exe"; do
  if [ -x "$cand" ]; then AG_BIN="$cand"; break; fi
done

echo ">> sealed-env E2E: the control plane delivers a secret it cannot read"
e2e_check_bins
[ -n "$AG_BIN" ] || { echo "the ag CLI is missing from $BIN"; exit 1; }

e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token

e2e_start_node sealed "e2e-sealed" "mock" "$TMP/work" "$TMP/repos" "$TMP/node"
e2e_wait_nodes_online 1 || { echo "node never came online"; tail -20 "$TMP/sealed.log"; exit 1; }

"$AG_BIN" --server "$BASE" login "$USER" "$PASS" >/dev/null

# --- 1. write through the CLI, from stdin ----------------------------------
echo ">> ag adapter-env set, value on stdin"
printf '%s' "$SECRET" | "$AG_BIN" --server "$BASE" adapter-env set '*' ANTHROPIC_API_KEY
printf '%s' "$BASEURL" | "$AG_BIN" --server "$BASE" adapter-env set '*' ANTHROPIC_BASE_URL

LS="$("$AG_BIN" --server "$BASE" adapter-env ls)"
printf '%s\n' "$LS" | sed 's/^/   | /'
printf '%s' "$LS" | grep -q 'sealed=1 pending=0' \
  || { echo "the entry was not sealed for the node (see ls above)"; exit 1; }
printf '%s' "$LS" | grep -q "fp=$(printf '%s' "$SECRET" | sha256sum | cut -c1-8)" \
  || { echo "the list does not carry the expected fingerprint"; exit 1; }
if printf '%s' "$LS" | grep -q "SUPERSECRET"; then
  echo "ag adapter-env ls printed the value"; exit 1
fi

# --- 2. the control plane never writes it -----------------------------------
echo ">> the plaintext is in none of the control plane's files"
BODY=$(curl -fsS "$BASE/v1/adapter-env" -H "authorization: Bearer $jwt")
if printf '%s' "$BODY" | grep -q "SUPERSECRET"; then
  echo "GET /v1/adapter-env returned the value"; exit 1
fi
printf '%s' "$BODY" | grep -q '"fingerprint"' \
  || { echo "the read path lost the fingerprint"; exit 1; }
for f in "$TMP/cp.db" "$TMP/cp.db-wal" "$TMP/cp.log"; do
  [ -e "$f" ] || continue
  hits=$(grep -a -c "SUPERSECRET" "$f" 2>/dev/null || true)
  echo "   $(basename "$f"): ${hits:-0} hits"
  [ "${hits:-0}" = "0" ] || { echo "the plaintext is in $f"; exit 1; }
done

# --- 3/4. the node opens it, the adapter sees it, masking is name-driven -----
echo ">> the adapter process receives the variable"
# One idle poll is ~1 s; give the node a couple of cycles to pick the entry up.
sleep 3
TID=$(e2e_submit $'env:ANTHROPIC_API_KEY\nenv:ANTHROPIC_BASE_URL' mock)
e2e_wait_terminal 120 "$TID" || { echo "task never finished"; tail -20 "$TMP/sealed.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "task status=$STATUS"; tail -20 "$TMP/sealed.log"; exit 1; }
echo "   task $TID: $STATUS"

LOG=$(e2e_task_events "$TID" | python3 -c '
import sys, json
d = json.load(sys.stdin)
items = d.get("items", d) if isinstance(d, dict) else d
for e in items:
    t = (e.get("payload") or {}).get("text")
    if isinstance(t, str):
        print(t)
')
printf '%s\n' "$LOG" | sed 's/^/   | /'

printf '%s' "$LOG" | grep -q 'env ANTHROPIC_API_KEY=\*\*\*' \
  || { echo "the secret did not reach the adapter (or was not masked)"; exit 1; }
printf '%s' "$LOG" | grep -q "env ANTHROPIC_BASE_URL=$BASEURL" \
  || { echo "the non-secret value did not reach the adapter verbatim"; exit 1; }
if printf '%s' "$LOG" | grep -q "SUPERSECRET"; then
  echo "the raw secret is in the event log"; exit 1
fi

# --- 5. removal revokes on a running node ----------------------------------
echo ">> ag adapter-env rm revokes it without restarting the node"
ID=$(printf '%s\n' "$LS" | sed -n 's/^# *\([0-9][0-9]*\) .*ANTHROPIC_API_KEY.*/\1/p' | head -1)
[ -n "$ID" ] || { echo "could not read the entry id from ls"; exit 1; }
"$AG_BIN" --server "$BASE" adapter-env rm "$ID"
sleep 3

TID2=$(e2e_submit "env:ANTHROPIC_API_KEY" mock)
e2e_wait_terminal 120 "$TID2" || { echo "second task never finished"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "second task status=$STATUS"; exit 1; }
LOG2=$(e2e_task_events "$TID2" | python3 -c '
import sys, json
d = json.load(sys.stdin)
items = d.get("items", d) if isinstance(d, dict) else d
for e in items:
    t = (e.get("payload") or {}).get("text")
    if isinstance(t, str):
        print(t)
')
printf '%s\n' "$LOG2" | sed 's/^/   | /'
printf '%s' "$LOG2" | grep -q 'env ANTHROPIC_API_KEY=<unset>' \
  || { echo "the value survived removal on the running node"; exit 1; }

echo "PASS: the CP sealed a secret it never stored, the node opened it, the adapter got it masked, and rm revoked it"
