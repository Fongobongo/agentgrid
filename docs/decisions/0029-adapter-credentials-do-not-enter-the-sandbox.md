# ADR 0029: Adapter credentials do not enter the sandbox

Status: **proposed** — nothing here is implemented. The threat it addresses is
split in [`threat-model.md`](threat-model.md) as **T13a** (the secret is
readable by the agent) and **T13b** (the agent acts with a credential it holds).
T13a closes for `gateway` entries when option F ships; T13b is not closable by
construction and is bounded instead.

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
through a logged stream is unaffected. **This is T13a, and the only exposure
this ADR is about.**

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

**A. Status quo.** T13a stays "not yet enforced" permanently, and every operator
who wants a hardened node invents the same workaround. Rejected as a
destination; this is the starting point.

**B. Move the value out of argv into a file or a tmpfs mount.** Rejected. It
addresses exposure 1, which the paragraph above argues is not the threat, and it
addresses only half of it (`docker inspect` still shows the value). It also
cannot help exposure 2: either the CLI reads the credential from a path — which
almost none do — or a wrapper exports it into the child's environment, at which
point the agent has it again. The one honest gain is smaller than the confusion
of shipping it under T13a's name.

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

**F. The hybrid: one sidecar, two modes, chosen per secret.** C3 and E are not
rivals once the gateway is the component that holds the key, because the only
thing E adds is a short-lived credential — and the gateway is the natural place
to mint one. The modes differ by one question: *can the consumer's traffic be
proxied?*

- **inject** (provider keys): the sandbox holds a per-attempt *capability*
  instead of a constant placeholder. It is opaque, bound to the attempt, with a
  TTL and a quota; the gateway validates it and adds the real credential on its
  own leg. The real key never leaves the gateway, and the sandbox's token is
  worthless anywhere else.
- **exchange** (secrets that have an exchange): the gateway mints a real,
  short-lived, provider-issued token and hands it to the sandbox, which then
  talks to the provider directly. Needed when the consumer cannot go through a
  proxy — git push is the case, and GitHub App installation tokens (one hour,
  scoped to a repository) are exactly this.

Why this beats E rather than repeating it: the credential authority is the
gateway, which already holds the key — no signing key on the daemon, no token
store with revocation semantics, no change to the adapter contract (the CLI
still receives an environment variable, exactly as it receives the placeholder
today). It also keeps E's genuine benefit where E is right, instead of
pretending provider keys have an exchange they do not.

What it adds over plain C:

- **Attribution.** Usage is counted per attempt at the provider boundary, not
  aggregated across whatever shares the node's key.
- **Revocation.** Cancelling an attempt revokes its capability; with a static
  placeholder the gateway cannot tell two attempts apart at all.
- **Quota before the call.** A per-attempt ceiling can refuse the next request,
  where today's budgets are computed from streamed events after the fact.
- **Budgets the agent cannot under-report.** Today cost comes from what the CLI
  prints; at the gateway it comes from the provider's own response.

What it costs: the sidecar becomes stateful (a token table or a persisted
signing key) and grows a small control API from the daemon (mint on attempt
start, revoke on end/cancel) — a new coupling and a new failure mode, since a
dead gateway now fails every provider call rather than merely removing a proxy.
One extra hop for SSE streams, and two code paths to test.

The asymmetry worth writing down: **inject mode is safe even with advisory
networking** — a capability is useless outside the gateway, so it does not
matter that the sandbox could dial the provider directly. **Exchange mode is
not**: the token it hands out is real, and it works from anywhere until it
expires. So exchange mode requires the enforced-egress fix above; inject mode
does not.

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

Staged, because each step is useful on its own and none of them requires the
next:

1. **Fix the egress topology first** (see the premise check below): two
   networks, so `restricted` means what the README says. Independent of this
   ADR and worth doing regardless — but exchange mode cannot ship without it.
2. **Ship the marker and the inject mode (F).** An entry marked `gateway` keeps
   the secret on the node, never places it in `container_env`, and the daemon
   **refuses** an attempt whose entries are `gateway` but whose `network_mode`
   is not `restricted` — the same fail-closed discipline as refusing an
   unenforceable allowlist. The marker must be explicit rather than a
   `*_KEY`/`*_TOKEN` name heuristic: a heuristic is exactly the "silently
   unenforced" state `validate_network_mode` exists to prevent (ADR 0023), and
   it fails in the direction that matters — a secret whose name does not match
   the pattern is handed to the agent and nobody is told.
3. **Add per-attempt capabilities** to the inject mode: an opaque token per
   attempt, bound to it, revocable on cancel, with a quota. This is where the
   accounting and the trustworthy budget come from; a constant placeholder is a
   working first version of step 2 and a dead end for everything else.
4. **Add the exchange mode** for secrets that have an exchange — starting with
   GitHub App installation tokens if and when git access moves off a static
   token. Not for provider keys, which have nothing to exchange.
5. **C3 (TLS + own CA) only if a CLI refuses an `http://` base URL.** It buys
   compatibility, not security: the agent's leg is inside the sandbox either way.
6. **Document D as the escape hatch**, for adapters that cannot take F, with its
   costs stated next to it. Not the default.
7. **Close T13a, do not claim T13b.** The split matters: T13a ("the secret is
   readable by the agent") closes for `gateway` entries and the row says which
   entries are still `env`. T13b ("the agent acts with a credential it holds")
   cannot close — acting is the product — so it is bounded by the quota, the
   revocation and the provider-boundary accounting, and described that way.
8. **Do not ship B**, and do not describe anything as T13a mitigation unless the
   value never enters the sandbox.

## Consequences

- Adapter env gains a delivery mode — a small syntax change on a knob operators
  already set (`AGENTGRID_ADAPTER_ENV`) — and a new failure mode at attempt
  start (`gateway` entry without `restricted` networking). Both are cheap to
  test and cheap to explain.
- F makes the egress sidecar mandatory for the hardened path, so a node without
  it can no longer claim a `gateway` entry. That is the point.
- Per-attempt capabilities turn the sidecar into a stateful component with a
  lifecycle hook from the daemon, and a dead gateway into a failed attempt
  rather than a missing convenience. That is the price of attribution and
  revocation, and it should be stated in the operator docs, not discovered.
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
