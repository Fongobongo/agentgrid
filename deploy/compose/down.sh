#!/usr/bin/env bash
# Stop the agentgrid stack. Keeps the SQLite/artifacts volume unless --purge.
# Pass --demo to target docker-compose.demo.yml instead of docker-compose.yml.
set -euo pipefail
cd "$(dirname "$0")/../.."
DEMO=0
PURGE=0
for a in "$@"; do
  case "$a" in --demo) DEMO=1;; --purge) PURGE=1;; esac
done
if [ "$DEMO" = "1" ]; then
  COMPOSE=(docker compose -f docker-compose.demo.yml)
  ENV_FILE="deploy/compose/.env.demo"
else
  COMPOSE=(docker compose -f docker-compose.yml)
  ENV_FILE="deploy/compose/.env"
fi
if [ "$PURGE" = "1" ]; then
  "${COMPOSE[@]}" down -v
  rm -f "$ENV_FILE"
else
  "${COMPOSE[@]}" down
fi
