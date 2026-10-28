# HA object-storage backend

`AGENTGRID_HA_REPL_BACKEND=s3` moves the HA **data plane** (snapshots,
incremental changesets, artifacts) to an S3-compatible bucket. Election, terms,
leases and votes still travel peer-to-peer; object storage never votes and a
bucket compromise cannot promote a replica.

Use it when control-plane hosts cannot reach each other on a private network but
can all reach a bucket, or when you want an off-host copy for disaster recovery.
SQLite itself still lives on local disk only — the bucket stores copied
snapshots/changesets, not a live database.

## Configuration

```bash
AGENTGRID_HA_REPL_BACKEND=s3
AGENTGRID_HA_S3_ENDPOINT=https://<project>.supabase.co/storage/v1/s3
AGENTGRID_HA_S3_BUCKET=agentgrid-ha
AGENTGRID_HA_S3_REGION=us-east-1
AGENTGRID_HA_S3_PREFIX=prod
AGENTGRID_HA_S3_ACCESS_KEY=...
AGENTGRID_HA_S3_SECRET_KEY=...
# optional: batches kept below the last snapshot (0 = never prune)
AGENTGRID_HA_CHANGESET_RETENTION=256
```

`AGENTGRID_HA_ALLOW_PLAINTEXT=1` is required only for a plaintext `http://`
endpoint (local MinIO/tests). Production buckets must use `https://`.

## Object layout

`<prefix>/<cluster_id>/` is the per-cluster namespace:

| Key | Content |
|---|---|
| `snapshots/<id>.db` | Consistent `VACUUM INTO` copy of the leader database |
| `snapshots/<id>.json` | Manifest: cluster, epoch, term, seq, schema, SHA-256, size |
| `snapshots/latest.json` | Advisory pointer to the newest manifest (written last) |
| `changesets/<first>-<last>.bin` | `AGCS` envelope + concatenated SQLite changesets |
| `artifacts/<sha256>` | Stored artifact bytes (the `.zst` frame is hashed as stored) |

A standby always trusts the manifest/envelope header, verifies size + SHA-256
and `quick_check`, and falls back to the latest snapshot when a changeset gap or
schema mismatch appears. Uploads are ordered database → manifest → `latest.json`
so a reader never sees a pointer to an incomplete snapshot.

### Retention and polling cost

- A standby lists changesets with `start-after` set to its own applied
  sequence, so a poll costs one LIST over *new* batches rather than over the
  cluster's whole write history.
- After each successful snapshot upload the leader deletes changeset batches
  fully contained in that snapshot, keeping the newest
  `AGENTGRID_HA_CHANGESET_RETENTION` batches (default `256`, `0` disables
  pruning). Without this the bucket gains one object per write batch forever.
- A standby that needs a pruned batch sees a gap and restores the snapshot
  instead; that is the documented fallback, not an error. Size the retention
  margin for the longest catch-up you want to serve without a snapshot
  (256 batches ≈ 4 minutes at the default 1 s async batching).

## Durability semantics

- `snapshot` RPO = `AGENTGRID_HA_SNAPSHOT_INTERVAL_SECS`.
- `async` RPO ≤ `AGENTGRID_HA_ASYNC_MAX_LAG_MS`; the leader demotes when no
  successful PUT lands inside the budget.
- `sync` counts a successful changeset PUT as the remote durable ack:
  self + object store = 2 data acknowledgements. A standalone bucket therefore
  satisfies the data majority for a 2- or 3-data-host topology; this is a
  deliberate trade-off (bucket durability replaces a second host's disk).
- An unreachable bucket in `sync` mode stops writes (the leader cannot promise
  RPO=0).

## Supabase recipe

1. Create a **private** Storage bucket (for example `agentgrid-ha`).
2. Create S3 access keys in Project Settings → Storage → S3 access keys.
   Scope the key to the bucket and the `<prefix>/` prefix.
3. Set `AGENTGRID_HA_S3_ENDPOINT` to the project's S3 endpoint shown next to the
   keys, `region` to the project region, and use the bucket name verbatim.
4. Keep public access disabled; only the control-plane hosts need
   `GetObject`, `PutObject`, `ListBucket` (prefix-scoped) and `DeleteObject`
   (maintenance/pruning).
5. Enable bucket versioning or a lifecycle rule if you want protection against
   an accidental overwrite of `snapshots/latest.json`.

MinIO and Cloudflare R2 use the same path-style client: point the endpoint at
the service URL, set the bucket/region, and keep TLS on. AWS S3 itself also
works with path-style requests.

## Shared JWT secret

HA requires the same `AGENTGRID_JWT_SECRET` (or
`AGENTGRID_HA_JWT_SECRET_FILE`) on every data host, otherwise a session issued
by one leader is rejected by the next. Each host publishes the SHA-256
fingerprint of its secret in `/v1/ha/status` and in the
`x-agentgrid-ha-jwt-fingerprint` peer header. A peer presenting a different
fingerprint is refused before it can vote or acknowledge writes; a witness has
no JWT secret and authenticates with the cluster token alone.

Provision the secret out of band (systemd `LoadCredential`, Ansible, sealed
secret). For a one-time bootstrap, set `AGENTGRID_HA_SECRET_FETCH=1` on the
leader and run:

```bash
ag cluster fetch-secret --out /etc/agentgrid/jwt-secret
```

The endpoint is leader-only, cluster-token authenticated, and requires TLS
unless plaintext was explicitly allowed. Disable `AGENTGRID_HA_SECRET_FETCH`
after provisioning; it is never replicated through SQLite.
