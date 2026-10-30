#!/usr/bin/env bash
# E2E (process-based, no Docker, no LLM): sessions in git.
#
# Proves the note actually lands in a repository: a real bare remote, a real
# clone by the node, a real attempt, and then `git notes` in the node's mirror.
# The unit tests cover the rendering and the git plumbing in isolation; this
# covers the wiring — that `record_session_note` is reached at all, with the
# right commit, after the attempt branch has already been reclaimed.
#
# Five assertions, in the order they can break:
#   1. the attempt succeeds and produced a commit;
#   2. refs/notes/agentgrid exists in the node's mirror;
#   3. the note hangs off that commit, and repeats its sha in the text;
#   4. the note carries the prompt and the adapter (that is the whole point);
#   5. refs/notes/commits is untouched, and with AGENTGRID_NOTES unset a later
#      attempt writes no note at all (off is the default, not a best effort).
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="${AGENTGRID_BIN:-$ROOT/target/debug}"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7829}"
PORT="${AGENTGRID_PORT:-7829}"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-notes-secret"
E2E_CONCURRENCY=2

TMP="$(mktemp -d -t ag-e2e-notes-XXXXXX)"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos" "$TMP/cfg"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

export XDG_CONFIG_HOME="$TMP/cfg"

# Git identity explicitly: CI runners have no global user.name, and a script
# that depends on one fails only on the hosts that matter (same lesson the
# notes feature itself learned).
GIT_ID=(-c user.name=e2e -c user.email=e2e@localhost)
gitq() { git "${GIT_ID[@]}" "$@"; }

echo ">> notes E2E: an attempt must leave its transcript on the commit it made"
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token

# --- a real remote the node can clone --------------------------------------
REMOTE="$TMP/remote.git"
gitq init -q --bare "$REMOTE"
gitq clone -q "$REMOTE" "$TMP/seed" 2>/dev/null
printf 'seed\n' >"$TMP/seed/README.md"
gitq -C "$TMP/seed" add .
gitq -C "$TMP/seed" commit -qm init
gitq -C "$TMP/seed" branch -M main
gitq -C "$TMP/seed" push -q origin main
# git wants a URL, and a Windows path is not one until the backslashes go.
URL="file:///$(printf '%s' "$REMOTE" | sed 's|^/c/|C:/|; s|^/d/|D:/|; s|\\|/|g')"
echo "   remote: $URL"

curl -fsS -o /dev/null -X POST "$BASE/v1/repositories" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d "{\"name\":\"notes-demo\",\"git_url\":\"$URL\",\"default_branch\":\"main\"}" \
  || { echo "repository registration failed"; exit 1; }

e2e_start_node notes "e2e-notes" "mock" "$TMP/work" "$TMP/repos" "$TMP/node" \
  AGENTGRID_NOTES=local
e2e_wait_nodes_online 1 || exit 1

echo ">> attempt with AGENTGRID_NOTES=local"
TID=$(e2e_submit "write:hello.txt:hi" mock notes-demo)
e2e_wait_terminal 120 "$TID" || { echo "task never finished"; tail -20 "$TMP/notes.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "task status=$STATUS"; tail -20 "$TMP/notes.log"; exit 1; }
echo "   task $TID: $STATUS"

MIRROR="$TMP/repos/notes-demo"
NOTES_REF="refs/notes/agentgrid"

echo ">> the mirror carries refs/notes/agentgrid"
gitq -C "$MIRROR" rev-parse --verify -q "$NOTES_REF" >/dev/null \
  || { echo "no $NOTES_REF in $MIRROR"; gitq -C "$MIRROR" for-each-ref; tail -20 "$TMP/notes.log"; exit 1; }

# `git notes list` prints "<note-blob-sha> <annotated-commit>"; the second
# column is the commit the note is attached to.
read -r _blob ANNOTATED < <(gitq -C "$MIRROR" notes --ref=agentgrid list | head -1)
[ -n "${ANNOTATED:-}" ] || { echo "notes ref exists but lists nothing"; exit 1; }
echo "   note on $ANNOTATED"

# The commit must be a real object in the mirror even though the attempt
# branch was reclaimed after the run.
gitq -C "$MIRROR" cat-file -e "${ANNOTATED}^{commit}" \
  || { echo "annotated commit $ANNOTATED is not in the mirror"; exit 1; }

NOTE=$(gitq -C "$MIRROR" notes --ref=agentgrid show "$ANNOTATED")
printf '%s\n' "$NOTE" | sed 's/^/   | /'

echo "$NOTE" | grep -q "^commit: $ANNOTATED$" \
  || { echo "the note does not repeat the sha it is attached to"; exit 1; }
echo "$NOTE" | grep -q "adapter: mock" \
  || { echo "the note does not name the adapter"; exit 1; }
echo "$NOTE" | grep -q "write:hello.txt:hi" \
  || { echo "the note does not carry the prompt"; exit 1; }
echo "$NOTE" | grep -q "ag events $TID" \
  || { echo "the note does not point at the full transcript"; exit 1; }

# Our own ref only: a user's notes live in refs/notes/commits and must not
# have been touched by a feature they never asked for.
if gitq -C "$MIRROR" rev-parse --verify -q refs/notes/commits >/dev/null; then
  echo "refs/notes/commits was created — that ref belongs to the user"; exit 1
fi

echo ">> a second attempt with AGENTGRID_NOTES unset writes no note"
e2e_kill_node
unset ENROLL_TOKEN
e2e_start_node notes "e2e-notes" "mock" "$TMP/work" "$TMP/repos" "$TMP/node"
e2e_wait_nodes_online 1 || exit 1
TID2=$(e2e_submit "write:bye.txt:bye" mock notes-demo)
e2e_wait_terminal 120 "$TID2" || { echo "second task never finished"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "second task status=$STATUS"; exit 1; }

COUNT=$(gitq -C "$MIRROR" notes --ref=agentgrid list | wc -l)
[ "$COUNT" -eq 1 ] || {
  echo "expected exactly 1 note after an unset AGENTGRID_NOTES run, found $COUNT"
  gitq -C "$MIRROR" notes --ref=agentgrid list
  exit 1
}

echo "PASS: the attempt note landed on its commit, off by default, own ref only"
