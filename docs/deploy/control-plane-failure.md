# Control-plane failure and recovery

The control plane (CP) is a single active instance over a local SQLite
database. A CP crash must never lose completed work: nodes buffer everything
in a durable outbox and the ingest path is idempotent.

## What happens when the CP dies

| Component | Behavior while the CP is down |
|---|---|
| Running attempts | Keep running on the node; the adapter is unaffected. |
| Event stream | Spooled to the node's durable outbox (`<data_dir>/outbox/events.jsonl`). |
| Completions | Recorded to `<data_dir>/outbox/completions.jsonl` before being sent. |
| Heartbeats | Fail silently; the node keeps retrying. |
| New tasks | Impossible (API is down); queued work resumes when the CP returns. |

On CP return, each node's flush loop redelivers from the outbox. Ingest is
idempotent: events use `ON CONFLICT (attempt_id, sequence) DO NOTHING`, and
completions are applied by attempt id, so redelivery never duplicates or
reorders data. Verified by `tests/e2e/run-outbox.sh` (kill -9, CP outage,
lost/retry, cancel scenarios).

Lease caveat: if the CP stays down longer than the ack/lease window (30 s),
restart may revert unacked assignments back to the queue. A node that already
started such an attempt will still report; the reconciliation on startup
(`reconcile_on_startup`) fixes drifted counters.

## Restarting the same instance

1. Restart the process with the **same** `AGENTGRID_DB`, `AGENTGRID_JWT_SECRET`
   and `AGENTGRID_ARTIFACT_ROOT`.
2. Check `/health/ready`, then `GET /v1/metrics` for
   `agentgrid_lease_reverts_total` / `agentgrid_active_attempt_drift_total`.
3. Nodes reconnect on their own; outboxes drain within seconds.

## Rebuilding from a backup (new host / lost data dir)

Backups are produced by `VACUUM INTO` and are consistent single-file copies:

- Manual: `POST /v1/admin/backup` with `{"path":"backup.db"}` (plain file
  name; lands in the data directory next to the artifact root).
- Automatic: the maintenance loop takes `auto-backup-<unix-ts>.db` every
  `AGENTGRID_BACKUP_EVERY_SECS` (default 86400) and keeps the newest
  `AGENTGRID_BACKUP_KEEP` (default 5). Watch
  `agentgrid_last_backup_age_seconds` (alert if it exceeds ~2× the cadence)
  and `agentgrid_backup_errors_total` (alert on any increase).

Restore procedure (validated end-to-end by `tests/e2e/run-restore.sh`):

1. Copy the backup file to the new host as the CP database
   (e.g. `cp backup.db cp.db`).
2. Start the CP with `AGENTGRID_DB` pointing at the copy. Users, tokens,
   tasks and attempts come back exactly as at backup time — log in with the
   pre-existing account (no setup-token bootstrap needed).
3. Old node credentials are only valid if the backup contains them and the
   `AGENTGRID_JWT_SECRET` matches; otherwise re-enroll nodes with fresh
   enrollment tokens (see `credential-rotation.md`).
4. Artifacts uploaded after the backup are not in it; the artifact root must
   be restored separately (filesystem snapshot/copy) if you need them.

Target: full replacement (start CP from backup + re-enroll a node + run a
task) completes in under 5 minutes.

## Do not

- Run two CPs against the same DB or on NFS/shared storage.
- Copy a live DB file while writes are in flight without `VACUUM INTO`
  (you get a torn copy); use the backup endpoint or stop the CP first.

## Graceful node drain (maintenance)

To take a node out of rotation without killing in-flight work:

1. `POST /v1/nodes/{id}/drain` with `{"drain": true}` (CLI: `ag node drain`).
   The scheduler stops assigning new attempts to the node; in-flight attempts
   run to completion and the heartbeat keeps the node `online`.
2. Watch `GET /v1/nodes/{id}` / the attempts list until the node has no
   `assigned|running|validating` attempts.
3. Stop the daemon, do the maintenance.
4. `POST /v1/nodes/{id}/drain` with `{"drain": false}` to accept work again;
   queued tasks are assigned on the next scheduler pass.

Verified end-to-end by `tests/e2e/run-drain.sh` (in-flight task finishes under
drain, new tasks stay queued, undrain resumes assignment).

