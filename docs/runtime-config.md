# Runtime configuration and diagnostics

AgentGrid daemons read one typed YAML config when `AGENTGRID_CONFIG` points
to a file. Explicit environment variables override file values, so existing
deployments keep working unchanged.

```yaml
control_plane:
  listen: 0.0.0.0:7800
  database: /var/lib/agentgrid/control-plane.db
  artifact_root: /var/lib/agentgrid/artifacts

node:
  server: http://127.0.0.1:7800
  node_name: node-1
  data_dir: /var/lib/agentgrid/data
  workspace_root: /var/lib/agentgrid/workspace
  repository_root: /var/lib/agentgrid/repos
  max_concurrency: 2
  heartbeat_secs: 10
  adapters: [mock]
  transport: auto
```

Unknown fields, invalid integers, unknown transports, and empty adapter lists
are startup errors. In `AGENTGRID_ENV=production`, control-plane and node
paths must resolve under `/var/lib/agentgrid`; this prevents a temp-dir
control plane from being started as a production instance.

Environment overrides:

| Variable | Config field |
|---|---|
| `AGENTGRID_LISTEN` | `control_plane.listen` |
| `AGENTGRID_DB` | `control_plane.database` |
| `AGENTGRID_ARTIFACT_ROOT` | `control_plane.artifact_root` |
| `AGENTGRID_SERVER` | `node.server` |
| `AGENTGRID_NODE_NAME` | `node.node_name` |
| `AGENTGRID_DATA_DIR` | `node.data_dir` |
| `AGENTGRID_WORKSPACE_ROOT` | `node.workspace_root` |
| `AGENTGRID_REPOSITORY_ROOT` | `node.repository_root` |
| `AGENTGRID_MAX_CONCURRENCY` | `node.max_concurrency` |
| `AGENTGRID_HEARTBEAT_SECS` | `node.heartbeat_secs` |
| `AGENTGRID_ADAPTERS` | `node.adapters` (comma-separated) |
| `AGENTGRID_TRANSPORT` | `node.transport` (`poll`, `ws`, `auto`) |

## Runtime doctor

`ag doctor runtime` inspects the local control-plane host without changing
anything:

- database path and size;
- WAL path and size;
- instance-lock inode and whether a live process holds it;
- free disk space;
- open file descriptors whose target was deleted;
- stale `ag-*` / `agentgrid-*` directories under the system temp dir.

Use it after a temp cleanup, disk-pressure alert, or unexpected daemon exit:

```bash
AGENTGRID_DB=/var/lib/agentgrid/control-plane.db ag doctor runtime
AGENTGRID_DB=/var/lib/agentgrid/control-plane.db ag doctor runtime --json
```

`ag doctor` (without `runtime`) remains the HTTP/server-side check.

## Shutdown and restore

- Control plane: SIGTERM drains HTTP requests, checkpoints WAL, closes the
  SQLite pool. Compose grants a 30-second stop window.
- Node daemon: SIGTERM stops new assignments, waits up to 60 seconds for
  in-flight attempts, then redelivers durable completion records.
- Backup/restore rehearsal: `bash tests/e2e/run-restore.sh` proves backup,
  fresh-instance restore, old task visibility, and a new successful task.
