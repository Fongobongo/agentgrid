#!/usr/bin/env bash
# Conformant stub for the CTX context-indexer CLI (Stage 11 contract, see
# `crates/node-daemon/src/zeroshot.rs`'s sibling `crates/node-daemon/src/ctx.rs`).
# Lets CI prove the node serves repeat attempts from the on-disk index cache
# without re-spawning the indexer; a real indexer implements the same two
# subcommands. Honest simplification: the "digest" is synthetic (no real
# indexing), but the invocation counting + JSON shape are exactly what the
# driver parses.
#
#   ctx-stub --version
#   ctx-stub index --repo <repo> --base <commit> --format json
#
# Every `index` call appends "<repo> <base>" to ${CTX_STUB_LOG} so E2E can
# assert the indexer ran exactly once across two same-key attempts.
set -euo pipefail

if [ "${1:-}" = "--version" ] || [ "${1:-}" = "-v" ]; then
  echo "ctx-stub 0.1.0"
  exit 0
fi

if [ "${1:-}" = "index" ]; then
  shift
  REPO=""; BASE=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --repo) REPO="$2"; shift 2;;
      --base) BASE="$2"; shift 2;;
      --format) shift 2;; # json only
      *) echo "stub: unknown index flag $1" >&2; exit 2;;
    esac
  done
  [ -n "${CTX_STUB_LOG:-}" ] && echo "$REPO $BASE" >>"$CTX_STUB_LOG"
  body="ctx-digest repo=$REPO base=${BASE:-none}"
  printf '{"body":"%s","bytes_in":1234,"bytes_out":0,"index_ms":5}\n' "$body"
  exit 0
fi

echo "stub: unknown subcommand ${1:-}" >&2
exit 2
