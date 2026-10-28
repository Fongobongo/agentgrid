# ADR 0021: Control-plane high availability

Status: accepted

## Context

The control plane (Axum + SQLite on local disk) is a single instance. A host
failure takes the API, scheduler and artifact store offline until the host
returns (ADR 0008 documented this as a deliberate ceiling; ADR 0001 constraint
#9 required no active-active SQLite). Users asked for HA: replicate the whole
control plane (API + scheduler + SQLite state + artifacts) on up to 3 real
hosts with configurable topology/RPO, while keeping single-host behavior
unchanged.

## Decision

**Active-passive cluster with peer/witness election, never active-active.**

- One leader (data host) serves all writes; standbys serve `/health/*` and
  `/v1/ha/*` only and reject data routes with `503` + `x-agentgrid-role` /
  `x-agentgrid-leader` hints.
- Election: term/epoch monotonicity, pre-vote, quorum leases that fence stale
  leaders, voters refuse to promote a candidate that is ahead. A witness is a
  vote/lease authority only — it stores no database and never acks writes.
- Topologies: 3 data hosts (2/3 quorum, sync quorum = 2 data hosts), or
  2 data hosts + 1 witness (witness cannot stand in for data freshness in
  sync mode). Single-host mode is byte-for-byte the old code path.
- Data plane: `VACUUM INTO` snapshots (SHA-256 + `quick_check` verified) plus
  optional incremental changesets (SQLite session extension, feature
  `ha-incremental`) for async (≤1s batched, demote above lag budget) or sync
  (data-quorum ack before reply, RPO=0 for acknowledged writes).
- Object-storage backend (`AGENTGRID_HA_REPL_BACKEND=s3`): snapshots,
  changesets and artifacts go to an S3-compatible bucket; election stays
  peer/witness. Sync mode counts a successful S3 PUT as a remote durable ack
  (self + object store = 2 data acks).
- Artifacts: eager push or on-read pull with SHA-256 verification.
- JWT secret identical on every data host; SHA-256 fingerprint published in
  `/v1/ha/status` and compared on peer auth; mismatched fingerprint refused.
  Opt-in one-time `ag cluster fetch-secret` over TLS + cluster token.
- Leader discovery: `GET /v1/ha/status` returns `role`. A leaderless cluster
  answers `503` + `Retry-After`.

**Stateless edge forwarder (`crates/edge`, binary `agentgrid-cp-edge`).**

- Discovers the leader via `/v1/ha/status` (cluster-token authenticated),
  short TTL cache, forwards HTTP/SSE/downloads unchanged, retries once after
  `503 not_leader` or connect error, answers `503` + `Retry-After` when no
  leader. Bridges the node WebSocket to the current leader. Holds no state;
  a VIP/DNS is an alternative, the CP's own `503` leader hint is the
  client-side failover path when no edge is deployed.
- Prometheus text metrics at `/metrics` (requests, discoveries, leaderless,
  retries, upstream errors, WS bridges).

## Consequences

- Single-node mode requires no external service/dep (unchanged).
- HA requires stable `AGENTGRID_JWT_SECRET` on every data host and
  `AGENTGRID_HA_CLUSTER_TOKEN` shared cluster-wide.
- Witness never stores data or acks writes; object storage never votes.
- No NFS/CIFS/network-attached DB or artifacts (still).
- Sync-mode writes need a data quorum; a witness vote alone is insufficient.
- S3 changeset listing starts after the standby's applied sequence, and the
  leader prunes batches already contained in its newest snapshot
  (`AGENTGRID_HA_CHANGESET_RETENTION`, default 256, `0` disables). A standby
  that needs a pruned batch falls back to the snapshot.

## Non-goals (kept from ADR 0001 / 0008)

- No active-active SQLite / multi-writer.
- No etcd/Consul (possible later election adapter, separate ADR).
- Pruning is leader-driven and sequence-based only: no bucket lifecycle rules,
  no per-replica acknowledgement tracking (a lagging replica degrades to a
  snapshot instead).

## Verification

- `cargo fmt --check`; `CARGO_INCREMENTAL=0 cargo clippy --workspace
  --all-targets -- -D warnings`; `CARGO_INCREMENTAL=0 cargo test --workspace`
  plus `--features ha-incremental`.
- 3 local processes (separate dirs/ports): elect, snapshot, promote, demote,
  no dual writers.
- Chaos e2e: `tests/e2e/run-ha-3host.sh`, `run-ha-2pluswitness.sh`,
  `run-ha-partition.sh` — kill/partition leader, exactly one leader, no
  acknowledged write loss, task completes, edge forwards, nodes reconnect.
  These run in CI (`.github/workflows/ha-chaos.yml`) on every change under
  `crates/control-plane/src/ha/`, `crates/edge/`, `crates/node-daemon/src/`
  or `tests/e2e/run-ha-*.sh`, nightly, and on PRs labelled `ha-chaos`.
- `cargo deny check` + `cargo audit`; musl release build with and without
  `ha-incremental`.
