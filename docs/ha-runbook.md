# HA runbook — agentgrid control plane

Operating an active-passive HA cluster: topologies, daily ops, failover,
recovery, and the edge forwarder.

## Topologies

| Topology | Voters | Write quorum (sync) | Tolerates |
|---|---|---|---|
| 3 data hosts | 3 data | 2 data | 1 host down |
| 2 data + 1 witness | 2 data + 1 witness | 2 data | 1 host down (witness can't stand in for data freshness in sync) |
| Single | 1 | — | none (default, unchanged) |

A witness votes and leases but stores no database and never acks writes.

## Configuration (per host)

Required on every data host:
- `AGENTGRID_HA_MODE=ha`
- `AGENTGRID_HA_ROLE=data` (witness: `witness`)
- `AGENTGRID_HA_NODE_ID=<unique>`
- `AGENTGRID_HA_CLUSTER_ID=<cluster>`
- `AGENTGRID_HA_CLUSTER_TOKEN=<shared >=32 bytes>`
- `AGENTGRID_HA_ADVERTISE_URL=https://<host>:7800`
- `AGENTGRID_HA_PEERS=<peer1@url,peer2@url>`
- `AGENTGRID_HA_WITNESSES=<witness@url,...>` (empty for data hosts)
- `AGENTGRID_JWT_SECRET=<identical >=32 bytes on every data host>`
- `AGENTGRID_HA_ALLOW_PLAINTEXT=1` only for dev `http://`; production uses TLS

Optional:
- `AGENTGRID_HA_RPO_MODE=snapshot|async|sync` (default snapshot)
- `AGENTGRID_HA_REPL_BACKEND=peer|s3` (default peer)
- `AGENTGRID_HA_S3_*` (endpoint, bucket, region, prefix, access_key, secret_key)
- `AGENTGRID_HA_CHANGESET_RETENTION` (batches kept below the last snapshot,
  default 256, `0` disables pruning — see `docs/ha-object-storage.md`)
- `AGENTGRID_HA_LEASE_TTL_SECS` (default 10), `AGENTGRID_HA_ELECTION_TIMEOUT_MS` (default 1500)
- `AGENTGRID_HA_SNAPSHOT_INTERVAL_SECS` (default 60)
- `AGENTGRID_HA_SECRET_FETCH=1` (leader-only one-time JWT secret provisioning; disable after)
- `AGENTGRID_HA_JWT_SECRET_FILE` (systemd `LoadCredential`, preferred for prod)

## Compose

- `deploy/compose/ha-3data.yml` — 3 control-plane data hosts + edge.
- `deploy/compose/ha-2pluswitness.yml` — 2 data + 1 witness + edge.

```
AGENTGRID_JWT_SECRET=<32+ bytes> AGENTGRID_HA_CLUSTER_TOKEN=<32+ bytes> \
  AGENTGRID_HA_CLUSTER_ID=prod \
  docker compose -f deploy/compose/ha-3data.yml up -d
```

Plaintext dev variants (all HTTP). Production: TLS in front of each control
plane, `https://` advertise URLs, S3 over TLS.

## Operator commands (`ag cluster`, authenticated with cluster token)

```
ag cluster status          # role, term, epoch, leader, applied_seq
ag cluster promote         # promote this data replica to leader (manual failover)
ag cluster demote          # step down from leader
ag cluster snapshot        # ask leader to write a snapshot now
ag cluster fetch-secret --out /etc/agentgrid/jwt-secret   # leader-only provisioning
```

`status` is also available at `GET /v1/ha/status` (cluster-token auth).

## Daily ops

### Check cluster health
```
curl -s http://cp-1:7800/health/ready        # leader: ok; standby: 503 + role hint
curl -s -H "x-agentgrid-ha-token: $TOKEN" http://cp-1:7800/v1/ha/status
```

### Failover (leader host dies)
1. Lease expires; remaining voters elect a new leader (target RTO <60s).
2. Clients/nodes follow the `x-agentgrid-leader` hint on `503`, or go through
   `agentgrid-cp-edge` which retries automatically.
3. The old host, on restart, rejoins as a standby (never a second leader).

### Manual failover
```
ag cluster demote        # on the current leader
# on the target standby:
ag cluster promote
```

### Rejoin a recovered host
Restart its control plane with the same config; it catches up from S3/peers
and becomes a standby. Verify `status` shows `role=standby` and `applied_seq`
advancing.

### Rotate the JWT secret
Provision the new secret out of band on every data host (same value), set
`AGENTGRID_HA_SECRET_FETCH=1` on the leader, run `ag cluster fetch-secret`,
then restart each host. Disable `AGENTGRID_HA_SECRET_FETCH` after.

## Edge forwarder (`agentgrid-cp-edge`)

```
AGENTGRID_EDGE_UPSTREAMS=http://cp-1:7800,http://cp-2:7800,http://cp-3:7800 \
AGENTGRID_HA_CLUSTER_TOKEN=<token> \
agentgrid-cp-edge --listen 0.0.0.0:7900
```

- `GET /health/live` — edge itself is up.
- `GET /health/ready` — a leader is reachable (else `503` + `Retry-After`).
- `GET /metrics` — Prometheus text: requests, discoveries, leaderless, retries,
  upstream errors, upstream timeouts, WS bridges.
- Falls back to proxying; on `503 not_leader`, connect error or a leader that
  stops answering it re-discovers and retries once.
- `AGENTGRID_EDGE_UPSTREAM_TIMEOUT_SECS` (default 10) bounds the wait for a
  response head. It deliberately covers only the head: SSE and artifact
  downloads keep streaming. Lower it if you want faster failover at the cost
  of giving up on slower endpoints.

## Chaos (tests/e2e)

- `run-ha-3host.sh` — 3 data hosts + edge; kill -9 leader; assert failover,
  edge forwarding, post-failover task, exactly one leader.
- `run-ha-2pluswitness.sh` — 2 data + witness + edge; same assertions.
- `run-ha-partition.sh` — SIGSTOP leader (partition), new leader elects,
  task completes via edge, SIGCONT heals, old leader steps down, no dual writers.

Run: `AG_E2E_KEEP=1 ./tests/e2e/run-ha-3host.sh` (keeps logs on failure).

## Alerts (Prometheus, from /metrics)

| Alert | Expression |
|---|---|
| No CP leader | `absent(agentgrid_cp_role{role="leader"})` or edge `agentgrid_edge_leaderless_total` increasing |
| Leader flapping | `rate(agentgrid_cp_role{role="leader"}[10m]) > 2` |
| Standby not catching up | standby `applied_seq` not increasing while leader advances |
| Edge upstream errors | `rate(agentgrid_edge_upstream_errors_total[5m]) > 0` |

Grafana dashboard: `deploy/grafana-dashboard.json`.

## Security notes

- `AGENTGRID_HA_CLUSTER_TOKEN` and `AGENTGRID_JWT_SECRET` are shared secrets;
  provision out of band (systemd `LoadCredential`, Ansible, sealed secret).
- Never commit `.env` or secrets; `AGENTGRID_HA_SECRET_FETCH` is one-time and
  disabled after provisioning.
- Object storage is data plane only — a bucket compromise cannot promote a
  replica; snapshot/changeset integrity is verified by SHA-256 + `quick_check`.
- Mismatched JWT fingerprint on a peer is refused before it can vote or ack.
