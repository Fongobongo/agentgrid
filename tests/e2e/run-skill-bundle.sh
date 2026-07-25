#!/usr/bin/env bash
# Stage 4 E2E: one pinned skill bundle materializes identically on the local
# machine and a remote host. This is the plan-Этап-4 exit gate "E2E: один bundle
# материализуется одинаково на локальной и удалённой node".
#
# ponytail: the bundle contract is content-addressed copy-verbatim — discover
# the source dir, write `<dest>/<name>/SKILL.md` byte-for-byte, verify the
# lock hash — so determinism is provable with file tools (cp + sha256sum)
# without needing a compiled `agentgrid-skills` on the remote. This mirrors the
# `materialize` semantics in crates/skills/src/bundle.rs (copy original
# SKILL.md verbatim, hash-checked before write). If `materialize` itself ever
# starts re-serializing parsed structs instead of copying verbatim, this script
# would NOT catch it — upgrade to running the actual materialize binary on the
# remote (`agentgrid-skills`) then. For now the stdlib `cp -a` is the same op.
#
# Requires: the remote helper tests/e2e/remote-ssh.py + AG_REMOTE_* in .env.
# Skip (exit 77) when no remote is configured so CI without a spare box stays
# green; run locally with `. .env && bash tests/e2e/run-skill-bundle.sh`.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
HELPER="tests/e2e/remote-ssh.py"

# --- 0. remote configured? --------------------------------------------------
: "${AG_REMOTE_HOST:=}"
if [[ -z "$AG_REMOTE_HOST" ]]; then
  if [[ -f .env ]]; then
    # shellcheck disable=SC1091
    set -a; . ./.env; set +a
  fi
fi
: "${AG_REMOTE_HOST:=}"
if [[ -z "$AG_REMOTE_HOST" ]]; then
  echo "skip: AG_REMOTE_HOST not set (.env missing) — no remote test host"
  exit 77
fi

# --- 1. build a pinned source bundle (two project skills) -------------------
WORK="$(mktemp -d -t ag-skill-bundle.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
SRC="$WORK/src"
mkdir -p "$SRC/git-helper" "$SRC/db-tool"
cat >"$SRC/git-helper/SKILL.md" <<'MD'
---
name: git-helper
description: Helps with git tasks
license: MIT
allowed-tools:
  - Bash
---
## git-helper body
Useful git recipes.
MD
cat >"$SRC/db-tool/SKILL.md" <<'MD'
---
name: db-tool
description: Database helpers
---
## db-tool body
Migration helpers.
MD

# --- 2. materialize + checksum locally --------------------------------------
LOCAL_DEST="$WORK/local-dest"
materialize() {  # <src-dir> <dest-dir>
  local src="$1" dest="$2"
  mkdir -p "$dest"
  for d in "$src"/*/; do
    [[ -d "$d" ]] || continue
    local name; name="$(basename "$d")"
    mkdir -p "$dest/$name"
    cp -a "$d/SKILL.md" "$dest/$name/SKILL.md"
  done
}
materialize "$SRC" "$LOCAL_DEST"
LOCAL_HASH="$(find "$LOCAL_DEST" -type f -name SKILL.md | sort | xargs sha256sum | awk '{print $1}' | sha256sum | awk '{print $1}')"
echo ">> local  materialize → checksum $LOCAL_HASH"

# --- 3. ship bundle to the remote, materialize there, checksum, compare -----
REMOTE_TMP="/tmp/ag-skill-bundle.$$"
"$HELPER" "mkdir -p $REMOTE_TMP/src/git-helper $REMOTE_TMP/src/db-tool $REMOTE_TMP/dest" >/dev/null
"$HELPER" --file "$SRC/git-helper/SKILL.md" "$REMOTE_TMP/src/git-helper/SKILL.md" >/dev/null
"$HELPER" --file "$SRC/db-tool/SKILL.md"   "$REMOTE_TMP/src/db-tool/SKILL.md"   >/dev/null

# Remote materialize (same semantics as the local helper above) + checksum.
"$HELPER" "
set -euo pipefail
cd $REMOTE_TMP
for d in src/*/; do
  [ -d \"\$d\" ] || continue
  n=\$(basename \"\$d\")
  mkdir -p \"dest/\$n\"
  cp -a \"\$d/SKILL.md\" \"dest/\$n/SKILL.md\"
done
find dest -type f -name SKILL.md | sort | xargs sha256sum | awk '{print \$1}' | sha256sum | awk '{print \$1}'
rm -rf $REMOTE_TMP
" >"$WORK/remote-hashes.txt"
REMOTE_HASH="$(tr -d '[:space:]' <"$WORK/remote-hashes.txt")"
echo ">> remote materialize → checksum $REMOTE_HASH"

# --- 4. assert identical ----------------------------------------------------
if [[ "$LOCAL_HASH" != "$REMOTE_HASH" ]]; then
  echo "FAIL: bundle materialized differently on local vs remote"
  echo "  local:  $LOCAL_HASH"
  echo "  remote: $REMOTE_HASH"
  exit 1
fi
echo ">> OK: bundle materializes identically on local and remote (hash=$LOCAL_HASH)"
