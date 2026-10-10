# ADR 0015: Secure profile — the shippable strict bundle

Status: accepted (Stage 12)

## Context

Stage 12 asks for a "secure profile": a ready-made binding of an isolated
backend and a strict policy, so an operator gets strong isolation without
hand-assembling a dozen knobs. All the pieces already exist; this ADR records
the bundle and its executable proof.

## Decision

The Secure profile is a **composition, not a new mechanism**:

1. **Node (isolated backend):** `AGENTGRID_SANDBOX=docker` (or `systemd`
   scope where containers are unavailable) with `AGENTGRID_SANDBOX_NETWORK=none`,
   `AGENTGRID_SANDBOX_READ_ONLY=1` (+ `AGENTGRID_SANDBOX_ARTIFACT_DIR` for
   outputs), per-attempt `ResourceLimits` from the active agent profile, and
   `AGENTGRID_RUN_AS_USER` for uid separation (Plan 6.11). Adapters run over
   ACP (`AdapterProtocol::Acp`, e.g. native launchers) so the node reports
   `permission_interception = "structured"`.
2. **Task:** `security_profile: "default-strict"` (any name ending in
   `-strict`). The scheduler refuses wrapper-only nodes with the visible
   reason `requires structured permission interception` (Hardening P0 item 5)
   — strict tasks only land where tool calls are intercepted.
3. **Profile:** agent profile with `autonomy` ≤ `l2` (destructive commands
   deny/ask per the 9.1 matrix), secret *requirements* (names only), and an
   `adapter_version` pin. L4 scheduled loops additionally require a template
   `budget` (Stage 13 ratify gate).
4. **Unsafe interlock:** `AGENTGRID_UNSAFE_UNATTENDED` is stripped on
   unsandboxed runs unless `AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX=1` AND
   `AGENTGRID_I_UNDERSTAND_UNSAFE=1` (startup refusal otherwise) — a Secure
   node never runs unattended-bypass by accident.

## Proof

`tests/e2e/run-secure-profile.sh` (CI e2e job): a `default-strict` task stays
`queued` on a wrapper-only node with the structured-interception reason on
`GET /v1/tasks/:id/eligibility`, then assigns to a structured (ACP) node and
reaches `succeeded`.

## Consequences

- Wrapper-only fleets cannot silently run strict tasks — the failure is a
  visible queue + reason, not a mis-executed task.
- "Secure" is a deployment posture (node env + task flag + profile), not a
  code fork: no new backend, no new scheduler predicate.
- The remaining gap for *provable* containment (seccomp/user-ns allowlists
  beyond cap-drop + `no-new-privileges`) is the CubeSandbox spike (ADR 0017).

## Operator recipe

```bash
# Node
AGENTGRID_SANDBOX=docker AGENTGRID_SANDBOX_NETWORK=none \
AGENTGRID_SANDBOX_READ_ONLY=1 AGENTGRID_SANDBOX_ARTIFACT_DIR=/var/lib/agentgrid/artifacts \
AGENTGRID_RUN_AS_USER=agentgrid-worker \
AGENTGRID_ADAPTERS="claude:acp,opencode:acp" agentgrid-node-daemon
# Task (any `-strict` suffix refuses wrapper-only nodes):
ag run "*" "audit the auth module" --adapter opencode --security-profile default-strict
```
