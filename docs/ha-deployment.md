# HA deployment guide — agentgrid control plane

Deploy a highly available active-passive control plane on up to 3 real hosts.

## Prerequisites

- A built `agentgrid-control-plane` binary on each host (release, musl
  recommended), plus `agentgrid-cp-edge` where an edge forwarder is wanted.
- A shared `AGENTGRID_HA_CLUSTER_TOKEN` (>=32 bytes) and identical
  `AGENTGRID_JWT_SECRET` (>=32 bytes) on every data host.
- Each data host has its own local SQLite DB + artifact dir (no NFS/network
  attached storage). A witness has no DB; its durable vote lives in
  `AGENTGRID_HA_WITNESS_DIR`.
- (Optional) S3-compatible bucket for object-storage replication
  (`AGENTGRID_HA_REPL_BACKEND=s3`).

## Install the control plane on a host

`deploy/install-control-plane.sh` creates the `agentgrid` user, data dirs,
systemd unit and starts the control plane:

```
./deploy/install-control-plane.sh \
  --listen 0.0.0.0:7800 \
  --data-dir /var/lib/agentgrid
```

Then add HA overrides via `systemctl edit agentgrid-control-plane` (or an
`EnvironmentFile=`):

```
Environment=AGENTGRID_HA_MODE=ha
Environment=AGENTGRID_HA_ROLE=data
Environment=AGENTGRID_HA_NODE_ID=cp-1
Environment=AGENTGRID_HA_CLUSTER_ID=prod
Environment=AGENTGRID_HA_CLUSTER_TOKEN=<shared token>
Environment=AGENTGRID_HA_ADVERTISE_URL=https://cp-1.example.com:7800
Environment=AGENTGRID_HA_PEERS=cp-2@https://cp-2.example.com:7800,cp-3@https://cp-3.example.com:7800
Environment=AGENTGRID_HA_WITNESSES=
Environment=AGENTGRID_HA_ALLOW_PLAINTEXT=0
Environment=AGENTGRID_HA_RPO_MODE=sync
Environment=AGENTGRID_HA_JWT_SECRET_FILE=/etc/agentgrid/jwt-secret
```

`AGENTGRID_JWT_SECRET` itself must be set separately (>=32 bytes) — never
baked into the unit; use `LoadCredential=` or an `EnvironmentFile=` with mode
0600.

The install script gained HA flags (`--ha-role`, `--ha-node-id`,
`--ha-cluster-id`, `--ha-cluster-token`, `--ha-peers`, `--ha-witnesses`,
`--ha-advertise-url`, `--ha-rpo-mode`, `--ha-snapshot-interval`,
`--ha-allow-plaintext`) that append the matching `Environment=` lines to the
unit.

## Provision the shared JWT secret

Preferred: out of band, identical on every data host (systemd `LoadCredential`,
Ansible, sealed secret). One-time bootstrap on the leader:

```
AGENTGRID_HA_SECRET_FETCH=1 systemctl restart agentgrid-control-plane
ag cluster fetch-secret --out /etc/agentgrid/jwt-secret
# copy to every data host with mode 0600, then:
systemctl unset-environment AGENTGRID_HA_SECRET_FETCH  # disable after provisioning
```

## Deploy the edge forwarder (optional but recommended)

```
./target/release/agentgrid-cp-edge \
  --listen 0.0.0.0:7900 \
  --upstreams https://cp-1.example.com:7800,https://cp-2.example.com:7800,https://cp-3.example.com:7800 \
  --cluster-token <token>
```

Or via Docker (`Dockerfile.edge`, image `agentgrid-cp-edge`). Clients and
nodes point at the edge; it forwards to the current leader and retries on
`503 not_leader`.

## Compose (dev / demo)

```
AGENTGRID_JWT_SECRET=<32+ bytes> AGENTGRID_HA_CLUSTER_TOKEN=<32+ bytes> \
  AGENTGRID_HA_CLUSTER_ID=prod \
  docker compose -f deploy/compose/ha-3data.yml up -d
```

- `ha-3data.yml`: 3 control-plane data hosts + edge.
- `ha-2pluswitness.yml`: 2 data + 1 witness + edge.

Both are plaintext dev variants. Production: TLS in front of each control
plane, `https://` advertise URLs, S3 over TLS.

## Verify

```
curl -s http://<host>:7800/health/ready
curl -s -H "x-agentgrid-ha-token: $TOKEN" http://<host>:7800/v1/ha/status
# expect role=leader on one host, standby on the others, same cluster/term
```

## Topology choice

- 3 data hosts: sync mode tolerates 1 host down with RPO=0 for acknowledged
  writes; recommended for production HA.
- 2 data + witness: cheaper; sync writes still need both data hosts (the
  witness cannot stand in for data freshness); async/snapshot tolerate one
  data host down.
