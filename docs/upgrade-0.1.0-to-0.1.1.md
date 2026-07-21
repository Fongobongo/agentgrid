# Upgrade Guide — 0.1.0 → 0.1.1

0.1.1 is a correctness & security hardening release. The data model and API are
backward compatible; no manual migration is required. This guide lists what
changed operationally and what to verify.

## Roll the control plane

1. Stop the 0.1.0 control plane (`docker compose down` or `systemctl stop
   agentgrid-control-plane`).
2. Replace the `agentgrid-control-plane` binary / image with the 0.1.1 build.
3. Ensure a **stable** `AGENTGRID_JWT_SECRET` is set (0.1.1 warns loudly if
   unset; a random-per-start secret invalidates node credentials on restart).
4. Start 0.1.1. `sqlx::migrate!` runs automatically at startup; no manual SQL.
   The SQLite WAL is checkpointed (`TRUNCATE`) on graceful shutdown and
   periodically; `quick_check` runs at boot.

## Roll the node daemon

1. Stop the 0.1.0 node daemon(s).
2. Replace the `agentgrid-node-daemon` binary / image with 0.1.1.
3. Ensure `AGENTGRID_DATA_DIR` is writable — 0.1.1 creates an `outbox/`
   subdirectory there for the durable event/completion spool.
4. Start 0.1.1 nodes. On startup they redeliver any completion a prior (killed)
   run recorded but the CP never acked (idempotent — safe against the 0.1.0
   state).

## Authentication changes

- The web UI now authenticates via an **HttpOnly + SameSite=Strict session
  cookie** (`agentgrid_token`), not a `localStorage` JWT. The browser client
  uses `credentials: include`; no code change is needed for the shipped UI.
- `POST /v1/auth/logout` clears the cookie.
- CLI / gateway / node auth via `Authorization: Bearer <jwt>` is **unchanged**
  and still works; the cookie is an additional path for browsers.
- Set `AGENTGRID_COOKIE_SECURE=1` to add the `Secure` attribute when serving
  over TLS.

## Operational additions

- New metrics on `/metrics`: `agentgrid_sqlite_checkpoint_ms`,
  `agentgrid_sqlite_busy_total` (in addition to the existing
  `agentgrid_sqlite_db_bytes` / `wal_bytes` / scheduler gauges).
- Disk-space guard: a node whose free disk falls below
  `AGENTGRID_DISK_LOW_MB` (default 1024) self-marks `degraded`.
- Protocol versioning: an incompatible node is marked
  `degraded(incompatible_protocol)` rather than silently misbehaving.

## Verify after upgrade

- `curl $BASE/health/ready` → 200.
- `ag nodes list` → nodes `online` (or `degraded` with a reason).
- Submit a test task; confirm it reaches `succeeded` and its events are
  contiguous (no sequence gaps).
- Run `tests/e2e/run-outbox.sh` (process-based, no Docker) against a throwaway
  DB to confirm durable delivery (kill -9 + network-disconnect scenarios).

## Rollback

0.1.1 writes no schema that 0.1.0 cannot read (additive migrations only). To
roll back, stop 0.1.1 and restart 0.1.0 binaries against the same SQLite file.
Any rows written by 0.1.1-only columns are ignored by 0.1.0. The durable
`outbox/` directory on nodes is 0.1.0-unused and can be left in place or
removed.
