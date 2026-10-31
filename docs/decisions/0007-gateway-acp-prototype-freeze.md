# ADR 0007: Gateway / ACP frozen at prototype status

Status: accepted (2026-08-06); **freeze lifted (amended 2026-10-31, see below)**

## Context

Alongside the core orchestrator (control plane + node daemon + adapters), the
repo contains two early components:

- `crates/acp` — a JSON-RPC 2.0 ACP codec, client/server, `session/update` →
  agentgrid event mapping, and a durable approval state machine.
- `crates/gateway` — the `agentgrid-gateway` binary built on it (southbound
  ACP client, northbound server; lets an ACP agent attach to the grid).

Both work in demos (`adapter-fake-acp` E2E, `agentgrid-acp-agent`) but they
are not exercised by any real-agent path and compete for engineering attention
with the MVP 0.2 critical path (real adapters, workflow reliability, ops).

## Decision

**Freeze gateway/ACP at current prototype status until MVP 0.2 is done.**

1. No new features for `crates/acp` or `crates/gateway`. Bug fixes only, and
   only for issues that break the workspace build or CI.
2. They stay in the workspace: `cargo build/test/clippy` keep covering them so
   they cannot bit-rot silently.
3. The `adapter-fake-acp` E2E stays in the suite as the regression net for
   the codec/state machine.
4. Revisit after the MVP 0.2 Definition of Done is met; candidates for the
   next cycle are a real ACP-agent E2E against a second host and promoting
   the gateway to a documented deployment artifact. *(Trigger fired; see the
   amendment at the end.)*

## Consequences

- Engineering effort concentrates on adapters/workflows/ops (the gaps named in
  `docs/plans/0.2-completion.md`).
- Users must not treat `agentgrid-gateway` as a supported deployment target
  yet; the README and quickstart describe the core stack only.
- If the ACP spec moves under us during the freeze, the codec may need a
  catch-up pass when the freeze lifts — accepted cost.

## Amendment (2026-10-31): the trigger fired, the freeze is lifted

Item 4 said "revisit after the MVP 0.2 Definition of Done is met". It was met
long ago — `docs/plans/0.2-completion.md` was deleted *because* it was fully
delivered (recorded in CHANGELOG), and the project is at v0.7.0. Nobody
revisited, so the ADR kept describing a freeze that had stopped matching the
tree, and two of its consequences had already become false:

- "the README and quickstart describe the core stack only" — the README has
  documented the Telegram gateway for several releases, marked **experimental**.
- "users must not treat it as a supported deployment target" — `agentgrid-gateway`
  is built and shipped in every release tarball, next to the core binaries.

Leaving the text as-is was the worst of both worlds: a reader could not tell
whether the freeze was active, and the one accurate word — *experimental* —
was in the README while the ADR said *frozen*.

The freeze is lifted. What that does and does not mean:

- **Still covered by CI** (`cargo build/test/clippy` over the whole workspace,
  plus the `adapter-fake-acp` E2E), so it cannot bit-rot silently — item 2 and
  item 3 stand.
- **Still experimental**, and the README is the source of truth for that word.
  Nothing here promises a stable northbound contract; the Telegram surface is
  what is documented, the ACP side is not.
- **Still unproven against a real agent.** The remaining gate is a real
  ACP-agent E2E against a second host — the original item 4 candidate. That is
  a verification task, not a freeze.

If the ACP spec moves before that E2E exists, the catch-up pass named above is
still the accepted cost.
