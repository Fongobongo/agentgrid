#!/usr/bin/env bash
# Disk hygiene for this dev box (see AGENTS.md "Disk discipline").
# Default: safe subset — Rust intermediate artifacts + stale /var/tmp leftovers
# (keeps target/debug binaries warm). Flags:
#   --full    also drop all of target/, web/dist, and web/node_modules
#   --docker  also `docker builder prune -f` (build cache only, no images)
#   --npm     also clear npm cache + npx cache
#   --days N  /var/tmp leftovers older than N days are removed (default 2)
set -euo pipefail

FULL=0; DOCKER=0; NPM=0; DAYS=2
while [ $# -gt 0 ]; do
  case "$1" in
    --full) FULL=1 ;;
    --docker) DOCKER=1 ;;
    --npm) NPM=1 ;;
    --days) DAYS="$2"; shift ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
  shift
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "== disk before =="
df -h / | tail -1

echo "== rust target =="
if [ "$FULL" = 1 ]; then
  rm -rf target web/dist web/node_modules
  echo "removed target/, web/dist, web/node_modules (full)"
else
  rm -rf target/debug/incremental target/debug/deps target/debug/build \
         target/release/incremental target/release/deps target/release/build
  echo "removed target/{debug,release}/{incremental,deps,build} (binaries kept)"
fi

echo "== /var/tmp leftovers older than ${DAYS}d =="
# agentgrid runtime leftovers: Vite SSR caches, ACP compile dirs, test spools.
find /var/tmp -maxdepth 1 -user "$(id -u)" -mtime +"$DAYS" \
  \( -name 'ag-*' -o -name 'ag.*' \) -print -exec rm -rf {} + 2>/dev/null || true
# random-hash dirs holding only a client/ tree (Vite SSR caches)
for d in /var/tmp/*/; do
  [ -d "$d" ] || continue
  if [ -d "${d}client" ] && [ "$(find "$d" -maxdepth 1 -mindepth 1 | wc -l)" = 1 ]; then
    if [ -z "$(find "$d" -maxdepth 0 -mtime -"$DAYS" 2>/dev/null)" ]; then
      [ -O "$d" ] && { echo "$d"; rm -rf "$d"; }
    fi
  fi
done

if [ "$NPM" = 1 ]; then
  npm cache clean --force >/dev/null 2>&1 || true
  rm -rf ~/.npm/_npx
  echo "npm cache + _npx cleared"
fi

if [ "$DOCKER" = 1 ]; then
  docker builder prune -f
fi

echo "== disk after =="
df -h / | tail -1
