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
   amendments at the end.)*

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
- **Now proven against a real binary, on a second host** (see the second
  amendment below). The gate that item 4 named is closed: a real
  `agentgrid-acp-agent` drives a real task on a real control plane, asserted
  from the control plane's side, and the whole thing runs in CI.

If the ACP spec moves before that E2E exists, the catch-up pass named above is
still the accepted cost.

## Second amendment (2026-10-31): the E2E gate is closed

The bullet above said the remaining gate was "a real ACP-agent E2E against a
second host". That was accurate at the time: `GatewayAgent` had four in-process
tests against a fake control plane, and the shipped binary had never spoken to
a real one.

`tests/e2e/run-acp-gateway.sh` now does exactly that, and it is registered in
the `e2e-failinject` CI job. It brings up a real control plane and a real node,
runs the real binary's JSON-RPC lifecycle over stdio (`initialize` →
`session/new` → `session/prompt`), and then — because the gateway's own
`succeeded` answer is not evidence — asks the control plane: the task exists
under the prompt that was sent, it has events, an event carries an
`attempt_id`, and one payload contains the prompt **as echoed by the node's
adapter**. That last assertion is what separates "the gateway answered" from
"the prompt reached an adapter and came back".

Both assertions were observed failing on purpose (a prompt absent from the CP
fails the lookup; a prompt never sent fails the echo check, printing the
payloads), so the test is not a green tick that cannot go red.

Run on two hosts, both green:

- Windows (debug build): 6 `session/update` notifications, `status=succeeded`.
- Debian 12, glibc 2.36, **the published `v0.7.0` musl tarball** — i.e. the
  artifact users actually download, `sha256sum -c` clean, all three binaries
  reporting `0.7.0`: 7 `session/update` notifications, and on the control
  plane `task 99d887c8-…: 7 events from 1 attempt(s), prompt echoed by the
  adapter`.

So "the second host" half of the gate is met with release artifacts, not a
local build. What is still *not* claimed: this is the ACP **agent** role that
Agentgrid implements and an external client drives; it is not a proof against
a third-party ACP agent implementation, and the northbound contract remains
experimental — the README is still the source of truth for that word.
