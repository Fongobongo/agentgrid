# Credential rotation runbook

Two secret classes exist: the CP signing secret (`AGENTGRID_JWT_SECRET`) and
per-node credentials (issued at enrollment, stored by the node in
`<AGENTGRID_DATA_DIR>/credential.json`). Enrollment tokens are one-time and
short-lived (10 min TTL, hash-only storage) — nothing to rotate there.

## Rotate `AGENTGRID_JWT_SECRET`

Rotating the secret invalidates **every** outstanding token in one shot:
user sessions and node credentials alike. Plan a maintenance window.

1. Take a backup first (`POST /v1/admin/backup`) — if the rotation goes
   wrong you can come back to the previous state.
2. Generate a new secret (≥32 random bytes), set it in the CP environment /
   compose file, and restart the control plane.
3. Users: log in again (old JWTs now fail signature verification).
4. Nodes: their saved credential no longer verifies. For each node:
   1. Stop the node daemon.
   2. Mint a fresh enrollment token: `POST /v1/nodes/enrollment-token`.
   3. Delete the stale credential: `rm <data_dir>/credential.json`
      (keep the outbox directory — any undelivered events still flush).
   4. Start the node with `AGENTGRID_ENROLL_TOKEN=<token>`; it re-enrolls,
      persists a new credential, and drains its outbox.
5. Verify: `GET /v1/nodes` shows every node `online`, outbox gauges
   (`agentgrid_node_outbox_rows`) drain to 0, and a smoke task succeeds.

No task or event data is lost by rotation itself: nodes keep their outbox and
redeliver once re-enrolled (ingest is idempotent).

## Revoke a single user session without rotating

Use session revocation (`revoked_sessions` by JWT `jti`) — e.g. after a
suspected token leak of one account. All other sessions stay valid.

## Retire a node credential

Delete/retire the node in the control plane and remove its `credential.json`;
there is no shared secret to change — each node credential is per-node.

## Operational notes

- Keep `AGENTGRID_JWT_SECRET` out of the database and out of logs; it lives
  only in the CP process environment (or your secret manager).
- If you restore a backup onto a CP running with a **different** secret, all
  node credentials from that backup stop working — re-enroll as above.
- Rotation is disruptive but safe: nothing in flight is lost, only
  re-authentication is required.
