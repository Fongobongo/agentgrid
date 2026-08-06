# ADR 0007: Gateway / ACP frozen at prototype status

Status: accepted (2026-08-06)

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
   the gateway to a documented deployment artifact.

## Consequences

- Engineering effort concentrates on adapters/workflows/ops (the gaps named in
  `docs/plans/0.2-completion.md`).
- Users must not treat `agentgrid-gateway` as a supported deployment target
  yet; the README and quickstart describe the core stack only.
- If the ACP spec moves under us during the freeze, the codec may need a
  catch-up pass when the freeze lifts — accepted cost.
