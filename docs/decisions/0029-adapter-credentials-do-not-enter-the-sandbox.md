# ADR 0029: Adapter credentials do not enter the sandbox

Status: **proposed** — nothing here is implemented. T13 in
[`threat-model.md`](threat-model.md) stays open until option C or D ships.

## Context

ADR 0028 sealed adapter env so that a compromised control plane cannot read the
values. The **node** still can, and it hands them to the sandbox:
`crates/node-daemon/src/sandbox.rs:911` turns every pair of `container_env` into
`--env K=V` on the `docker run` command line.

`container_env` is assembled in `main.rs:119-160` and mixes secrets with
ordinary configuration:

| source | example | secret? |
|---|---|---|
| `AGENTGRID_ADAPTER_ENV` (operator) | `ANTHROPIC_API_KEY=sk-ant-…` | yes |
| proxy pool (`cfg.proxies.current()`) | `HTTPS_PROXY=http://…` | no (may carry `user:pass`) |
| CP-managed adapter env (`managed_adapter_env`) | a custom base URL | depends |
| profile `secret_requirements` | whatever the profile names | yes |
| `AGENTGRID_SYSTEM_PROMPT`, `AG_GROUP_ID` | — | no |

The daemon does not distinguish the rows: every one becomes an argv pair. Two
different exposures follow, and they must not be conflated — conflating them is
how this threat stayed open.

### Exposure 1: the value is visible on the node

`ANTHROPIC_API_KEY=sk-ant-…` sits in the docker client's argv (any process
listing shows it) and in the container's config (any client of the docker socket
shows it).

This is **not fixable at the sandbox boundary, and not worth a design**: the
node must hold the plaintext to use it — it is in the daemon's own environment,
in `cfg.adapter_env`, and in the argv of the `docker run` it spawns. Anyone who
can read the node's process table can also read the daemon's `/proc/<pid>/environ`.
Moving the value into an `--env-file` or a tmpfs mount would remove the argv copy
and leave the `docker inspect` copy (docker copies the file's contents into the
container config; `--env-file` is a convenience for the client, not a secrecy
mechanism), while `inspect` access is already root-equivalent on the node. The
mitigation for exposure 1 is node access control, which is T14/T25 territory, not
this ADR.

### Exposure 2: the agent owns the value

The agent process is the container's main process. It can print
`$ANTHROPIC_API_KEY`, copy it into the worktree, or send it to any host it can
reach. Output masking (T1) cannot help: masking filters what is *logged*, while
the value is genuinely present, and any exfiltration path that does not pass
through a logged stream is unaffected. **This is the exposure T13 names, and the
only one this ADR is about.**

Two things sharpen it. `network_mode=restricted` gives the sandbox exactly one
reachable peer — the egress proxy (ADR 0012/0023) — so the set of destinations a
stolen key can reach is small; but the provider endpoint is *by definition* on
that list, so a stolen key is usable. And the "secure profile" posture (ADR 0015)
cannot honestly claim to contain secrets while handing the secret to the process
it is meant to contain.

What is **not** the problem: the control plane. With sealed env (ADR 0028) it
stores ciphertext and cannot read values back. The boundary in question is
node → sandbox.

## Options

**A. Status quo.** T13 stays "not yet enforced" permanently, and every operator
who wants a hardened node invents the same workaround. Rejected as a
destination; this is the starting point.

**B. Move the value out of argv into a file or a tmpfs mount.** Rejected. It
addresses exposure 1, which the paragraph above argues is not the threat, and it
addresses only half of it (`docker inspect` still shows the value). It also
cannot help exposure 2: either the CLI reads the credential from a path — which
almost none do — or a wrapper exports it into the child's environment, at which
point the agent has it again. The one honest gain is smaller than the confusion
of shipping it under T13's name.

**C. A credential gateway the sandbox talks to instead of the provider.** The
sandbox gets a placeholder (`ANTHROPIC_API_KEY=agentgrid-gateway`) and a base URL
pointing at a process on the egress network that terminates the sandbox's
connection and injects the real credential on its own outbound leg. No TLS
interception is needed, because the *sandbox's* leg is plain HTTP to the gateway
and the gateway's leg to the provider is an ordinary HTTPS client.

- Works for any adapter whose CLI honours a base-URL environment variable. The
  adapters forward env verbatim, so this is already deployable by hand today —
  what is missing is a way to say "this value must never reach the sandbox" and
  a guarantee that the sandbox can reach the gateway at all.
- Does not work for an adapter whose CLI hardcodes the provider endpoint.
- Requires the gateway to be the sandbox's only peer in `restricted` mode, i.e.
  it lives on the egress network beside squid, not on the host's loopback
  (`bwrap` with `--unshare-net` keeps only its own loopback — the constraint
  ADR 0023 ran into).

**D. `ssl_bump` + `request_header_add` in squid.** Works for every HTTPS client
with no adapter cooperation, including ones with a hardcoded endpoint. Costs: a
CA the sandbox must trust (in the image and in the agent's own trust store), a
proxy that can read all sandbox TLS, and anything with certificate pinning
breaks. Largest blast radius of the four; the only one that covers adapters C
cannot.

**E. A credential broker.** The adapter asks the daemon for a short-lived
credential instead of holding a long-lived key. It is the most appealing option
on paper and it does not work as a replacement for C, for one structural
reason: **our adapter is a wrapper, not the API client.** `adapter-claude.rs:210`
spawns `claude`; `claude` makes the provider call. A broker can only hand its
token to that child — i.e. into the environment this whole ADR is about. E does
not remove the credential from the sandbox, it shortens its life.

The rest follows:

- **No exchange exists.** Anthropic and OpenAI do not mint short-lived scoped
  tokens from an API key. So "short-lived" can only mean a token our own
  component validates — which is a gateway. E without a gateway is not
  implementable; E with one is C plus a token layer.
- **TTL against run length.** Attempts run for minutes to hours and the wrapped
  CLI cannot refresh. A TTL shorter than the run fails mid-attempt with a 401;
  a TTL longer than the run is a leak window barely smaller than a permanent
  key.
- **The daemon becomes a credential authority**: a signing key, a store of
  issued tokens, revocation, clock skew, restart semantics (in-memory tokens
  vanish), and a much better target — compromising the daemon stops meaning
  "read the key" and starts meaning "mint one".
- **Accounting is lost.** With C every provider call passes one gateway that
  can count tokens, enforce quotas and log. With E the CLI talks to the provider
  directly; the daemon sees a mint, not a spend. That was C's second-largest
  benefit.
- **It covers the same subset** — nothing for git tokens, registry credentials,
  SSH keys or MCP tokens, none of which can be exchanged for a short-lived one.
- **It inherits the broken premise** below: without enforced egress the token is
  still exfiltratable and usable until expiry.
- **It changes the adapter contract**, the least versioned part of the system
  (five wrappers plus the ACP path, and we have just been through
  `NODE_PROTOCOL_VERSION=2`), and needs a fail-closed test per adapter.
- Practical: no unix socket on Windows nodes, and some CLIs cache the credential
  into `~/.config` inside the container, which a broker does not prevent.

Where E is genuinely right: secrets that *have* an exchange — GitHub App
installation tokens (one hour, scoped to a repository) are exactly E, and for
git access C is not needed at all. That is a reason to implement E **for those
secrets**, not to use it for provider API keys.

## Premise check: `restricted` is not enforced today

C and E both lean on `restricted` meaning "one reachable peer". It does not, in
the shipped deploy. `resolved_network_mode` (`sandbox.rs:505`) attaches the
sandbox to `$AGENTGRID_SANDBOX_EGRESS_NETWORK`, and
`deploy/egress-proxy/docker-compose.yml` declares that network `internal: false`
— a user-defined bridge with NAT, so a process that ignores `HTTP_PROXY` reaches
the internet and the LAN directly. `deploy/egress-proxy/README.md` promises
"internet to allowlisted domains only, no LAN", which is precisely the
silently-unenforced state ADR 0023 exists to prevent.

Consequence for this ADR: the gate in the decision ("`gateway` without
`restricted` is refused") stays correct — it guarantees the sandbox can reach
the gateway — but the *reason* changes from "the gateway is the only peer" to
"the gateway is the only peer we put there". Fixing the topology (two networks:
`internal: true` for squid + sandboxes, a second one for squid only) is a
prerequisite, and it is worth doing on its own because the README currently
claims a guarantee the compose file does not provide.

## Decision (recommended)

1. **Implement C, gated on an explicit marker.** The marker must be explicit
   rather than a `*_KEY`/`*_TOKEN` name heuristic: a heuristic is exactly the
   "silently unenforced" state `validate_network_mode` exists to prevent
   (ADR 0023), and it fails in the direction that matters — a secret whose name
   does not match the pattern is handed to the agent and nobody is told.
   Concretely: an entry marked `gateway` keeps the secret on the node, never
   places it in `container_env`, and the daemon **refuses** an attempt whose
   entries are `gateway` but whose `network_mode` is not `restricted` — the same
   fail-closed discipline as refusing an unenforceable allowlist.
2. **Document D as the escape hatch**, for adapters that cannot take C, with its
   costs stated next to it. Not the default.
3. **Change T13's row to an honest partial**: "enforced for `gateway` entries;
   `env` entries are still handed to the agent and say so".
4. **Do not ship B**, and do not describe anything as T13 mitigation unless the
   value never enters the sandbox.

## Consequences

- Adapter env gains a delivery mode — a small syntax change on a knob operators
  already set (`AGENTGRID_ADAPTER_ENV`) — and a new failure mode at attempt
  start (`gateway` entry without `restricted` networking). Both are cheap to
  test and cheap to explain.
- C makes the egress sidecar mandatory for the hardened path, so a node without
  it can no longer claim a `gateway` entry. That is the point.
- D, if adopted later, puts a CA in every sandbox image: a supply-chain and
  trust-store change, not a config flag.
- None of this makes `env` mode safe. The end state is that the exposure is
  *named per entry* instead of being uniform and undocumented.

## Revisit when

- An adapter that needs D exists (C blocked by a hardcoded endpoint), which
  turns D from an escape hatch into a requirement.
- Or a provider starts issuing short-lived scoped tokens derived from a
  long-lived key. That makes option E implementable without a gateway and would
  shrink the window for `env` entries too, without pretending they are safe.
