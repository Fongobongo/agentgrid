# Egress firewall for restricted-mode sandboxes

Docker/Podman have no native per-domain egress filter, so the node daemon
collapses `network_mode=restricted` to `--network none` (strictly more
isolated than promised, never less). This sidecar restores "internet but
allowlisted domains only" for restricted attempts.

## How it works

1. Start the proxy (edits `squid.allowlist.conf` first):

       docker compose -f deploy/egress-proxy/docker-compose.yml up -d

2. Run the node daemon with the egress network + proxy URL:

       AGENTGRID_SANDBOX_EGRESS_NETWORK=agentgrid-egress \
       AGENTGRID_SANDBOX_EGRESS_PROXY=http://egress-proxy:3128 \
         agentgrid-node-daemon

3. Tasks with `network_mode=restricted` now attach to `agentgrid-egress` and
   get `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` injected; the proxy permits only
   the allowlisted domains and blocks LAN ranges (RFC1918) by default. Without
   the two env vars, restricted keeps the old behaviour (`--network none`).

## Two networks, and why

`agentgrid-egress` is `internal: true`. A sandbox attached to it has exactly one
reachable peer — this sidecar — and no route to anything else: a process that
ignores `HTTP_PROXY` and opens a raw socket gets `ENETUNREACH`. The sidecar is
also the only member of `agentgrid-egress-outbound`, which is where its own
default route comes from.

This is not decoration. An earlier version of this file declared a single
network with `internal: false`; a sandbox attached to it had NAT of its own, so
the allowlist applied only to processes that chose to use the proxy, and the
"internet to allowlisted domains only" line below was false. If you are
upgrading, recreate the stack — the network definition is part of the guarantee.

The daemon cannot verify the topology for you (it only knows a network name), so
`tests/e2e/run-egress-isolation.sh` asserts it from the outside: a container on
`agentgrid-egress` must fail to reach the internet without the proxy.

## Isolation guarantees

- **restricted + proxy**: internet to allowlisted domains only, no LAN, and no
  direct route that bypasses the proxy.
- **restricted, no proxy**: no network at all (fail-closed, current default).
- **none / unrestricted**: unchanged — a `bridge` attempt never inherits the
  proxy env (the node only injects it for restricted attempts on the egress
  network).
- The heartbeat's `enforced_limits` stays honest: a restricted attempt on the
  proxy network still counts as egress-isolated, so the flag does not drop.

The proxy runs read-only, cap-dropped, `no-new-privileges`, with caching
disabled (agent responses must be fresh; caching could leak one attempt's
fetched content into another).
