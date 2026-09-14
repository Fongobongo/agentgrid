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
```

## Grafana

A ready dashboard covering the panels above ships at
`deploy/grafana-dashboard.json` (import in Grafana → Dashboards → Import;
point it at your Prometheus datasource). Requires no extra plugins.
