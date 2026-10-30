# ADR 0023: Per-task egress allowlist — no-go in the current shape

Status: spike evaluated (backlog item 2) — **no-go**; the operator-global
egress proxy stays the enforced path

## Context

`network_mode` is already a per-task field (`none` | `restricted` |
`unrestricted`), and `validate_network_mode` already *parses* an
`allowlist:<cidr>,…` spec — then refuses it at startup, because no backend can
enforce it. The gap is real: a task cannot say "only `api.github.com`", it can
only say "restricted", and `restricted` means "whatever the operator put in
`deploy/egress-proxy/squid.allowlist.conf`" (or nothing at all).

## What enforcement would actually require

Two mechanisms exist, and only one of them is enforcement:

1. **Network-level** — the agent's namespace contains exactly one reachable
   peer (the proxy). Everything else is unroutable. This is what the current
   docker + `--internal` network + squid sidecar does, and it is genuinely
   enforced: a process that ignores `HTTP_PROXY` and opens a raw socket gets
   `ENETUNREACH`.
2. **Policy-level** — keep full egress and inject `HTTP_PROXY`. This is
   advisory, not enforcement: any adapter, script or tool call can dial the
   destination directly. Reporting it as an allowlist would be exactly the
   "silently unenforced" state `validate_network_mode` exists to prevent.

Per-task enforcement therefore means *per-attempt* network isolation:

- **docker:** create an `--internal` network and a squid container with a
  generated config per attempt, then tear both down. Real, but the cost is
  per attempt (network create ~0.2 s, squid ~0.3 s and ~20–40 MB RSS, plus
  config generation), and a daemon crash leaves orphan networks that need the
  same cleanup discipline as orphan containers. It also multiplies the
  container count per node, which the RSS budgets and the node's concurrency
  ceiling were not designed around.
- **bwrap / none:** there is no veth and no root, so `--unshare-net` leaves
  only the sandbox's own loopback — a host-side proxy is unreachable. Sharing
  the host netns and injecting `HTTP_PROXY` is mechanism 2, i.e. advisory.
  A real user-mode network stack (`slirp4netns` / `pasta`) would work, but it
  is a new external dependency and a redesign of the netns construction, not a
  flag on the existing argv.

## Decision

- **No per-task allowlist.** `allowlist:` stays parsed-but-refused: a typo'd
  CIDR fails at startup and a valid one fails with "not enforceable", so no
  configuration can produce a filter that only exists in the documentation.
- The enforced path remains the operator-global egress proxy (ADR 0012),
  reached by `network_mode=restricted`. Per-task narrowing is an operator
  decision made in `squid.allowlist.conf`, where it is actually enforced.
- The backlog item is closed as declined, with the two upgrade paths recorded
  above rather than left as a standing "todo" that cannot be honestly done.

## Revisit when

Either (a) per-attempt docker networks are measured to be acceptable on the
reference node (the `tests/e2e/measure-sandbox-coldstart.sh` harness is the
place to measure it), or (b) `pasta`/`slirp4netns` is accepted as a node
dependency, which would make a user-mode netns with a single reachable proxy
the unprivileged equivalent of the docker design.
