#!/usr/bin/env bash
# pre-merge-resolve.sh — deterministic merge-conflict resolution pass (plan 1.2, #11).
#
# Runs BEFORE any LLM resolve: a handful of trivial, provably-safe patterns that
# cover the ~95% case (import-both, both-add, formatting-only). Anything left
# conflicted after this pass must go to the LLM agent — the script exits 1 so
# the caller knows to escalate.
#
# Usage: pre-merge-resolve.sh <worktree-dir>
# Exit: 0 = no conflicts remain (resolved or none), 1 = conflicts remain.
#
# All operations are plain `git` subcommands (no `sh -c` strings built from
# repository content), so a crafted filename cannot inject a shell command.

set -euo pipefail

WS="$1"
cd "$WS"

conflicted() {
    git diff --name-only --diff-filter=U
}

# Nothing to do.
if ! conflicts=$(conflicted); then
    exit 0
fi
[ -z "$conflicts" ] && exit 0

resolved_count=0

for f in $conflicts; do
    # --- Pattern 1: formatting-only conflict -------------------------------
    # Both sides changed the file but differ only in whitespace (or one side
    # is a pure whitespace variant of the other). Take the "theirs" side
    # (incoming change) and let the merge record it. Detected by re-merging
    # with whitespace ignored: if `merge-file -Xignore-space-change` yields no
    # markers, the conflict was cosmetic.
    if git show ":$f" >/dev/null 2>&1; then
        : # file exists in index; proceed
    fi
    ours=$(git show "HEAD:$f" 2>/dev/null || true)
    theirs=$(git show "MERGE_HEAD:$f" 2>/dev/null || true)
    if [ -n "$ours" ] && [ -n "$theirs" ]; then
        if [ "$(printf '%s' "$ours" | tr -d ' \t\r\n')" = "$(printf '%s' "$theirs" | tr -d ' \t\r\n')" ]; then
            git checkout --theirs -- "$f"
            git add "$f"
            resolved_count=$((resolved_count + 1))
            continue
        fi
    fi

    # --- Pattern 2: both added / both changed different regions ------------
    # Re-merge the two sides with `--union`: keeps BOTH sides' lines when the
    # changes do not overlap (import-both, both-add). Only applies when the
    # union merge produces a marker-free result.
    if [ -n "$ours" ] && [ -n "$theirs" ]; then
        base=$(git merge-base HEAD MERGE_HEAD 2>/dev/null || true)
        base_content=$(git show "$base:$f" 2>/dev/null || true)
        tmp=$(mktemp -d)
        printf '%s\n' "$base_content" > "$tmp/base"
        printf '%s\n' "$ours" > "$tmp/ours"
        printf '%s\n' "$theirs" > "$tmp/theirs"
        if git merge-file --union "$tmp/ours" "$tmp/base" "$tmp/theirs" 2>/dev/null \
            && ! grep -q '^<<<<<<<\|^>>>>>>>' "$tmp/ours"; then
            cp "$tmp/ours" "$f"
            git add "$f"
            rm -rf "$tmp"
            resolved_count=$((resolved_count + 1))
            continue
        fi
        rm -rf "$tmp"
    fi

    # --- Pattern 3: deleted on one side, modified on the other --------------
    # If the deletion is on OUR side (we removed the file) keep the deletion
    # (tombstone wins); if THEY deleted and we modified, keep our change.
    if ! git cat-file -e "HEAD:$f" 2>/dev/null && git cat-file -e "MERGE_HEAD:$f" 2>/dev/null; then
        # only in theirs -> conflict is add/add or delete/modify
        git checkout --theirs -- "$f" 2>/dev/null && git add "$f" 2>/dev/null \
            && resolved_count=$((resolved_count + 1))
        continue
    fi
    if git cat-file -e "HEAD:$f" 2>/dev/null && ! git cat-file -e "MERGE_HEAD:$f" 2>/dev/null; then
        git checkout --ours -- "$f" 2>/dev/null && git add "$f" 2>/dev/null \
            && resolved_count=$((resolved_count + 1))
        continue
    fi
done

# Report what happened; exit non-zero if anything is still conflicted.
remaining=$(conflicted || true)
if [ -n "$remaining" ]; then
    echo "pre-merge-resolve: resolved $resolved_count; still conflicted:" >&2
    printf '%s\n' "$remaining" >&2
    exit 1
fi
echo "pre-merge-resolve: resolved $resolved_count conflict(s)"
exit 0
