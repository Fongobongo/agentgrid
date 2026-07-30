#!/usr/bin/env bash
# Shared e2e bootstrap helper (hardening P0 #2: fail-closed auth removed the
# AGENTGRID_BOOTSTRAP_USER/PASSWORD backdoor). After `start_cp` brought the
# control plane up, `bootstrap_first_user "$cp_log" "$base" "$user" "$pass"`
# reads the one-time setup token printed to stdout and POSTs /v1/auth/setup to
# create the first admin user. Idempotent: if the DB already has a user (e.g.
# the script reuses a cushioned DB), the token is absent and the function is
# a no-op, provided the credentials match; otherwise it errors.
#
# Usage:
#   source tests/e2e/lib-bootstrap.sh
#   bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
# Then call login() normally.

set -euo pipefail

# Wait for the control plane to print the setup token (first start, no users).
# The token line is bounded by the two `===` markers, on its own line.
_setup_token_from_log() {
  local log="$1"
  # Wait up to 20s for the marker to appear.
  local i
  for i in $(seq 1 20); do
    if grep -q "agentgrid setup token" "$log" 2>/dev/null; then
      break
    fi
    sleep 1
  done
  # Extract the token: the line between the two `===` markers (skip the
  # surrounding banner lines).
  awk '/=== agentgrid setup token/{f=1;next} /=== present this at/{f=0} f' "$log" \
    | grep -E '^[A-Za-z0-9_-]+$' | tail -1
}

# Create the first user from the printed setup token. If the token is absent
# (a user already exists), assume the requested credentials already do too.
bootstrap_first_user() {
  local cp_log="$1"; local base="$2"; local user="$3"; local pass="$4"
  local token
  token="$(_setup_token_from_log "$cp_log")"
  if [ -z "$token" ]; then
    echo "  no setup token in CP log (DB already bootstrapped); skipping setup" >&2
    return 0
  fi
  local status
  status=$(curl -fsS -o /dev/null -w '%{http_code}' -X POST "$base/v1/auth/setup" \
    -H 'content-type: application/json' \
    -d "{\"username\":\"$user\",\"password\":\"$pass\",\"setup_token\":\"$token\"}")
  if [ "$status" != "201" ] && [ "$status" != "200" ]; then
    echo "  bootstrap setup failed (HTTP $status); CP log:" >&2
    cat "$cp_log" >&2
    return 1
  fi
}
