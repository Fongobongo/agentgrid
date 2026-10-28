# Prometheus + Grafana setup (Plan 3.10 follow-up)

The control plane exposes a Prometheus text-format scrape endpoint at
`GET /metrics` (public, no auth — same class as `/health/*`; put it behind
your reverse proxy's access rules if the port is exposed).

## Prometheus scrape config

```yaml
# prometheus.yml
scrape_configs:
  - job_name: agentgrid-cp
    scrape_interval: 15s
    static_configs:
      - targets: ["cp-host:7800"]   # or the TLS-terminated proxy in front
```

Verify by hand:

```bash
curl -s http://cp-host:7800/metrics | head -20
curl -s http://cp-host:7800/health/ready
```

## Metric reference (subset)

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `agentgrid_tasks` | gauge | `status` | Current task counts by status. |
| `agentgrid_nodes` | gauge | `status` | Current node counts by status (`online`/`degraded`/`offline`/…). |
| `agentgrid_tasks_total` | counter | `status` | Cumulative terminal task outcomes. |
| `agentgrid_task_errors_total` | counter | `error_code` | Failed/cancelled tasks by error code (`resource_limit:memory`, `agent_failed`, `validation_failed`, `node_lost`, `timeout`, …; empty = unclassified). |
| `agentgrid_task_duration_seconds` | histogram | — | Duration of finished tasks. |
| `agentgrid_scheduler_latency_ms` | gauge | — | Last queued→assignment latency. |
| `agentgrid_oldest_queued_task_seconds` | gauge | — | Queue starvation watch. |
| `agentgrid_node_free_disk_mb` / `agentgrid_node_mem_available_mb` / `agentgrid_node_load_avg` | gauge | `node` | Host vitals from heartbeats. |
| `agentgrid_node_outbox_bytes` / `agentgrid_node_outbox_rows` | gauge | `node` | Durable delivery backlog per node (nonzero + growing = CP unreachable). |
| `agentgrid_sqlite_wal_bytes` / `agentgrid_sqlite_db_bytes` | gauge | — | Storage growth. |
| `agentgrid_sqlite_write_lock_failures_total` / `agentgrid_sqlite_busy_total` | counter | — | SQLite write contention. |
| `agentgrid_last_backup_age_seconds` / `agentgrid_backup_errors_total` | gauge / counter | — | Automatic backup health. |
| `agentgrid_cross_node_rejects_total` / `agentgrid_stale_fencing_tokens_total` | counter | — | Fencing/cross-node safety net trips. |
| `agentgrid_validation_outcomes_total` | counter | `outcome` | Post-agent validation pass/fail. |

### HA cluster metrics (only in `AGENTGRID_HA_MODE=ha`)

Single mode emits none of these, so existing dashboards and alerts are
unaffected.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `agentgrid_cp_role` | gauge | `role` | `1` for the instance's current role (`leader`/`standby`/`witness`/`single`), `0` otherwise. |
| `agentgrid_cp_term` | gauge | — | Election term; a jump means a failover happened. |
| `agentgrid_cp_epoch` | gauge | — | Fencing generation. |
| `agentgrid_ha_lease_remaining_seconds` | gauge | — | Leader quorum lease left; `0` on a standby. |
| `agentgrid_ha_fenced_writes_total` | counter | — | Writes refused by the lease/epoch gate. |
| `agentgrid_cp_applied_seq` | gauge | — | Highest replication sequence applied locally. |
| `agentgrid_cp_snapshot_seq` / `agentgrid_cp_snapshot_age_seconds` | gauge | — | Newest local snapshot's sequence and age. |
| `agentgrid_cp_has_leader` / `agentgrid_cp_leader_contact_age_seconds` | gauge | — | Whether a leader is known, and how long since it last confirmed. |
| `agentgrid_cp_need_full_snapshot` | gauge | — | `1` while the replica is waiting for a snapshot instead of applying changesets. |
| `agentgrid_cp_changeset_batches_applied_total` / `agentgrid_cp_changeset_apply_failures_total` | counter | — | Incremental replication progress, and rejections/gaps. |

The edge forwarder (`agentgrid-cp-edge`) exposes its own counters on
`/metrics`: `agentgrid_edge_requests_total`,
`agentgrid_edge_leader_discoveries_total`, `agentgrid_edge_leaderless_total`,
`agentgrid_edge_retries_total`, `agentgrid_edge_upstream_errors_total`,
`agentgrid_edge_upstream_timeouts_total`, `agentgrid_edge_ws_bridges_total`.
Scrape it as a second job — `leaderless_total` growing is the earliest signal
that clients are seeing `503`; `upstream_timeouts_total` growing means the
cached leader stopped answering and the edge is burning its retry budget
(`AGENTGRID_EDGE_UPSTREAM_TIMEOUT_SECS`, default 10).

## Alerting suggestions

```yaml
groups:
  - name: agentgrid
    rules:
      - alert: AgentgridQueueStarving
        expr: agentgrid_oldest_queued_task_seconds > 300
        for: 5m
      - alert: AgentgridNodeDeliveryBacklog
        expr: agentgrid_node_outbox_bytes > 10 * 1024 * 1024
        for: 10m
      - alert: AgentgridBackupStale
        expr: agentgrid_last_backup_age_seconds > 25 * 3600
        for: 30m
      - alert: AgentgridWriteLockContention
        expr: rate(agentgrid_sqlite_write_lock_failures_total[10m]) > 0
        for: 10m
      # --- HA (ADR 0021) ---
      # Every instance scrapes its own /metrics; `sum` collapses the cluster.
      - alert: AgentgridHALeaderless
        # No instance claims leadership for 30 s: clients get 503.
        expr: sum(agentgrid_cp_role{role="leader"}) < 1
        for: 30s
      - alert: AgentgridHADualLeaders
        # Two leaders in one term is the split-brain symptom the fencing
        # gate exists to prevent; page immediately.
        expr: sum(agentgrid_cp_role{role="leader"}) > 1
        for: 15s
      - alert: AgentgridHAReplicationStalled
        # A standby's applied_seq stopped advancing while the leader keeps
        # writing.
        expr: |
          max(agentgrid_cp_applied_seq) - min(agentgrid_cp_applied_seq) > 100
        for: 5m
      - alert: AgentgridHASnapshotStale
        # Snapshots are cut every AGENTGRID_HA_SNAPSHOT_INTERVAL_SECS (60 s
        # default); 10x that means replication has silently fallen back to
        # nothing.
        expr: agentgrid_cp_snapshot_age_seconds > 600
        for: 10m
      - alert: AgentgridHAChangesetFailures
        expr: rate(agentgrid_cp_changeset_apply_failures_total[10m]) > 0
        for: 10m
      - alert: AgentgridEdgeLeaderless
        expr: rate(agentgrid_edge_leaderless_total[5m]) > 0
        for: 5m
```

## Grafana

The full dashboard ships at `deploy/grafana/agentgrid.json` (import in
Grafana → Dashboards → Import; point it at your Prometheus datasource). It
covers every panel above plus the HA row (role/term, replication progress,
liveness, changeset apply and fencing). No extra plugins required.

`deploy/grafana-dashboard.json` is the older eight-panel cut and is kept only
for setups that already imported it; it has no HA panels.
