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
  outcomes (cut off with one relay, carried on with two).
- **A `502` rotates, but only after three in a row.** A relay that is alive but
  has lost its own route answers `502 control-plane unreachable` — and so does
  any egress proxy in front of a dead control plane. Neither is a transport
  error: the connection was established, so `is_connect()` never sees it, and
  before this the node stayed pinned while a working endpoint sat unused in its
  list. The fix is a counter rather than "rotate on 502", because the earlier
  objection was to rotating on a *single* bad gateway — a load balancer hiccup
  would then shuffle the whole fleet. Three consecutive `502`/`504` from one
  endpoint is a real failure; the counter is shared by poll, heartbeat and ws
  (same endpoint, so three failures across them mean the same thing) and is
  cleared by any successful answer and by every endpoint change. `503` keeps
  its own meaning (standby, follow the hint) and other `5xx`/`4xx` do not
  rotate at all: a `500` is the control plane answering, and a `401` is not
  fixed by moving.
- Enrollment applies the same rule with no threshold: it retries the next
  endpoint on a gateway error instead of failing boot. It runs once, over a
  bounded number of attempts, so there is no thrashing to avoid.
- The relay's `/metrics` and `/health/*` shadow the control plane's paths on
  that port (the same trade the standalone edge already makes).
- `agentgrid-node-daemon` now links `axum` (through the edge library). The rest
  of the HTTP stack was already there via `reqwest`.
- Control-plane replication could be routed through a relay in front of the
  leader by pointing peer URLs at it, but that is untested and not the primary
  case; it is mentioned so nobody assumes it was validated.

## Revisit when

- ~~**More than one hop is needed** (A → B → control plane). The upstream is a
  single URL today; chaining relays needs a hop counter to detect loops,
  because the current self-loop check only catches a direct self-reference.~~
  **Resolved (amendment below): relays chain with no extra configuration, and
  a hop counter cuts a loop with `508`.**
- **`tokio-tungstenite` gains proxy support**, or the node grows its own
  `CONNECT` client. Then the proxy pool could carry WS, and a relay could be
  replaced by an ordinary HTTP proxy the operator already runs.
- ~~**Three proves too few or too many in practice.**~~ **Resolved.** The
  threshold is now `AGENTGRID_GATEWAY_FAILURES_BEFORE_ROTATING`, defaulting to
  `GATEWAY_FAILURES_BEFORE_ROTATING` (3). Values below 2 are refused and fall
  back to the default with a warning, because "a single bad gateway must not
  move a node" is the whole point of the counter; the constant keeps its
  `const _: () = assert!` so the default cannot be edited to 1 either. It is
  documented in `docs/agent-ops.md` next to the relay section — the bullet
  above was right that an undocumented knob is worse than a constant.

## Amendment (2026-10-31): chains are supported, and a loop is cut with 508

The revisit item above said chaining "needs a hop counter to detect loops".
Two things turned out to be true, and only one of them needed code.

**Chaining needs no configuration.** A relay forwards to `cfg.base()` — the
control plane *this node* would talk to right now. A stranded node's base is
already the relay that carries it, so a node that is itself carried can relay
for someone further out, and the chain grows by pointing the next node at it:

```
node C ──▶ relay B ──▶ relay A ──▶ control plane
(knows B)            (knows A)
```

C is not told about A, B is not told about C, and no environment variable
describes the topology. That fell out of the original `Fixed(|| cfg.base())`
decision; the ADR's "one hop only" was a statement about what had been tested,
not about what the design prevented.

**The loop is what needed code.** The static check (`relays_to_itself`) only
catches a relay whose upstream is itself. Two operators pointing relays at
each other — or three in a ring — produce no startup error: every relay binds,
every relay accepts, and a request that enters the ring is forwarded forever
while the fleet stops heartbeating. The failure looks like a hang, which is
the worst kind to debug.

So a counter that travels *with the request*:

- every relay increments `x-agentgrid-relay-hops` before forwarding, and
  **replaces** whatever the client sent (a client cannot reset its own count
  by sending `0`, and a later hop cannot be tricked by an earlier one);
- at `AGENTGRID_RELAY_MAX_HOPS` (default **3**) a relay answers `508 Loop
  Detected` before reading the body, with a message naming the header, the
  count and the limit, and increments `agentgrid_edge_loop_detected_total`;
- `0` is refused at startup — a relay that answers `508` to everything is a
  relay that listens and is useless, and the operator would meet it as a
  `508` storm rather than as a config error;
- a non-numeric or negative value reads as "0 hops so far" rather than being
  rejected, because a hand-edited header must not be able to *skip* the guard.

The counter lives in the forwarder (`crates/edge`) rather than in the node
daemon, because the daemon's relay and the standalone edge are the same
forwarder and the WebSocket bridge is inside it — putting the guard in a layer
means the HTTP fallback and `/v1/node/ws` are both covered without either
knowing the header exists. `EdgeConfig::max_hops = None` leaves the request
untouched: the standalone edge in front of a control plane is the first hop of
a chain that has length one, and rewriting headers it has no reason to rewrite
is how a proxy starts breaking the server behind it.

### Proof

- `crates/edge`: 21 tests (was 17). Four new: the arithmetic (`0`/absent →
  1, at-limit → refused, nonsense → 0), and three that go through the real
  layer stack on a real socket — a fresh request arrives upstream as `1`, one
  entering with `2` arrives as `3`, one entering at the limit is answered
  `508` **with the upstream hit count still zero** (it never got forwarded),
  and with `max_hops = None` a client-sent `7` arrives as `7` and no header is
  invented. Two of the four were watched failing with the layer removed; the
  arithmetic test correctly kept passing, since it does not depend on the
  layer.
- `crates/node-daemon/src/relay.rs`: 9 tests (was 6). `AGENTGRID_RELAY_MAX_HOPS`
  without `AGENTGRID_RELAY_LISTEN` is a startup error, `0` is refused with a
  message that says what it would do, and a non-numeric value is an error
  rather than a silent default.
- `tests/e2e/run-relay-chain.sh`, registered in CI: three nodes on one host,
  C configured with **only** B's relay and B with **only** A's. Asserts that C
  enrolls through both hops, that a task pinned to C succeeds, that both
  relays carried requests with no loop recorded, and then the discriminating
  pair — the same request entering B with `1` hop is forwarded (A sees 2),
  while entering with `2` is answered `508` with `relay loop` in the body and
  A's counter at 1. With `max_hops` forced to `None` in the daemon, steps 1–5
  still pass and step 6 fails (`got 404`) — which is the honest shape of the
  change: the chain worked before, and the counter is what stops it from
  becoming a ring.

Still not claimed: every process in the E2E is on `127.0.0.1`, so "C cannot
reach the control plane" is configuration, not a firewall — the same caveat as
the one-hop E2E. And a relay still forwards only to its own control plane or
to the relay that carries it; it is not a general proxy and does not accept a
destination from the request.
