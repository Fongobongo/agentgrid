# ADR 0026: Relay through an intermediate host

Status: accepted

## Context

Not every host in a grid can reach every other host. The cases that actually
show up:

- a node on a segment with no route to the control plane (a firewall rule, a
  NAT that does not hairpin, a VPC peering that was never opened), while a
  sibling node in the same rack reaches it fine;
- a control-plane replica that cannot dial a peer across a segment;
- an operator's laptop running `ag` from a network that only reaches one host.

Today the answer is "run `agentgrid-cp-edge` on some middle host". That works,
and it is the right answer when the middle host is a load balancer. It has two
limits:

- the middle host must have our binary installed — and if it is a node, it
  already runs `agentgrid-node-daemon`, so the operator installs a *second*
  program on a host that was already qualified;
- the edge discovers the leader with `GET /v1/ha/status` and the **cluster
  token**. A single-control-plane deployment has no cluster token at all, so
  the edge cannot be deployed there, which is exactly the small deployment
  where hand-rolling a relay is most tempting.

There is also a consumer-side mechanism that looks like it should solve this:
the node's egress proxy pool (ADR 0012) with CP-pushed `proxy_urls`. It does
not, for two reasons. It is a *consumer* — someone still has to run the proxy —
and it only covers the HTTP client: the WebSocket transport dials
`tokio-tungstenite::connect_async` directly and has no proxy support, so a
node told to use a proxy silently degrades to polling (or fails outright under
`AGENTGRID_TRANSPORT=ws`).

## Decision

Make a **node** able to be the middle host, using the forwarder the edge
already has.

1. The forwarding core moves out of `crates/edge/src/main.rs` into the crate's
   library: `Edge`, `EdgeConfig`, `router`, `serve`, and the `Cidr` type. The
   `agentgrid-cp-edge` binary becomes a thin wrapper that parses flags and
   builds one upstream. No forwarding behaviour changes.

2. `Upstream` becomes an enum with two variants:
   - `Ha` — the existing behaviour: discover the leader among configured
     replicas with the cluster token, cached for `LEADER_TTL`;
   - `Fixed` — a closure the caller supplies, evaluated **per request**.

3. `AGENTGRID_RELAY_LISTEN=0.0.0.0:7900` on a node starts the same forwarder
   with `Upstream::Fixed(|| cfg.base())`. This is the whole trick: the node's
   transport already follows `X-AgentGrid-Leader` hints and mutates its
   endpoint list, so `base()` is already "the control plane we would talk to
   right now". The relay therefore follows a leader move on the *next
   request* — no TTL, no cluster token, no second discovery mechanism.

4. `AGENTGRID_RELAY_ALLOW=10.0.0.0/8,192.168.1.5` bounds who may use it. The
   gate runs as middleware before any body is read, on `ConnectInfo`, so it
   cannot be spoofed with a header. Empty means "any source" — correct for a
   standalone edge behind an operator's own policy, wrong for a node, so the
   daemon logs a warning when it starts a relay without a list.

5. Fail-loud startup, in the shape the rest of the daemon uses:
   - `AGENTGRID_RELAY_ALLOW` without `AGENTGRID_RELAY_LISTEN` is a config
     error (the operator expected a relay; nothing would listen);
   - a listen address that would forward to itself (same host:port as the
     control plane, or an unspecified address on the same port) is refused —
     the loop would look like a hang;
   - a port that is already taken aborts the daemon, because a relay that
     silently never ran breaks peers hours later instead of now.

6. Nothing on the client side changes. A peer points at the relay with
   `AGENTGRID_SERVER=http://node-a.internal:7900`, and `poll`, `ws` and `auto`
   all work: the forwarder proxies HTTP, streams SSE and artifacts, and bridges
   `/v1/node/ws`.

### Why not a CONNECT proxy

It would be more general and it is what the proxy pool already consumes. But
`tokio-tungstenite` has no proxy support, so `ws` would need its own `CONNECT`
implementation plus a TLS-over-tunnel handshake — more new code in the client
than in the forwarder. And an open `CONNECT` proxy is a worse security posture
than a forwarder whose only possible destination is the control plane it was
configured with.

### Why not a mesh or an overlay

The need is one hop, in one direction, with the control plane as the only
destination. An overlay (WireGuard, a service mesh) is a better answer for
general host-to-host connectivity, and it is a deployment decision, not a
feature of this daemon. The relay is deliberately not a step towards one.

## Proof

- `crates/edge`: 17 tests. The 12 that existed still pass unchanged; the new
  ones pin the two properties the relay depends on — a `Fixed` upstream never
  calls `/v1/ha/status`, and it is re-read per request (a leader move between
  two requests lands on the new leader, not after a TTL) — plus CIDR parsing
  and matching, including that a v4 entry never matches a v6 peer.
- `crates/node-daemon/src/relay.rs`: 6 tests — env parsing, allowlist without
  listen, self-loop detection, URL authority parsing.
- `tests/e2e/run-relay.sh`, registered in CI: a real control plane, node A
  with the relay, node B configured with **only** A's address. It asserts the
  allowlist returns 403 for a peer outside it, that B enrolls through the
  relay, that a task **pinned to B** succeeds, that the relay's counters move,
  that the WebSocket transport also works through the relay, and that killing
  A makes B report the loss of its only route.

## Consequences

- A middle host that is already a node needs one environment variable and no
  second binary, and no cluster token.
- The relay widens who can reach the control plane — that is the feature — so
  it is off by default and `AGENTGRID_RELAY_ALLOW` is the boundary. It does
  **not** bypass authentication: `Authorization` is forwarded verbatim and the
  control plane still authenticates the peer's own node credential.
- One hop only. A relay forwards to its own control plane, not to another
  relay.
- **Redundancy comes from the node's endpoint list, not from discovery.**
  `AGENTGRID_SERVERS=relay-a,relay-b` reuses the rotation that already exists for
  HA replicas, so a node moves onto the second relay when the first stops
  answering. Nothing discovers a relay, and a node configured with a single
  `AGENTGRID_SERVER` has nowhere to go — which is why the e2e asserts both
  outcomes (cut off with one relay, carried on with two). The rotation triggers
  on a connect/timeout error, on `503` and on demotion, **not** on `502`: a relay
  that is alive but has lost its own route answers `502`, and so does any egress
  proxy in front of a dead one, so those cases stay pinned. Making `502` rotate
  is a change to HA semantics (it would also move nodes off a CP replica that
  merely had a bad gateway) and is deliberately not made here.
- The relay's `/metrics` and `/health/*` shadow the control plane's paths on
  that port (the same trade the standalone edge already makes).
- `agentgrid-node-daemon` now links `axum` (through the edge library). The rest
  of the HTTP stack was already there via `reqwest`.
- Control-plane replication could be routed through a relay in front of the
  leader by pointing peer URLs at it, but that is untested and not the primary
  case; it is mentioned so nobody assumes it was validated.

## Revisit when

- **More than one hop is needed** (A → B → control plane). The upstream is a
  single URL today; chaining relays needs a hop counter to detect loops,
  because the current self-loop check only catches a direct self-reference.
- **`tokio-tungstenite` gains proxy support**, or the node grows its own
  `CONNECT` client. Then the proxy pool could carry WS, and a relay could be
  replaced by an ordinary HTTP proxy the operator already runs.
