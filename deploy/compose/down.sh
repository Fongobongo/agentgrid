#!/usr/bin/env bash
# Stop the agentgrid stack. Keeps the SQLite/artifacts volume unless --purge.
set -euo pipefail
cd "$(dirname "$0")/../.."
if [ "${1:-}" = "--purge" ]; then
  docker compose down -v
  rm -f deploy/compose/.env
else
  docker compose down
fi
