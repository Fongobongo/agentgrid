# ADR 0016 (spike): h5i executor/provenance — go/no-go

Status: spike evaluated (Stage 12) — **no-go for bundling, go for the contract**

## Context

Stage 12 asks for an h5i spike: evaluate h5i as an executor with provenance,
and write a go/no-go. h5i is an external system (like Zeroshot/CTX): no binary
was available to evaluate in this environment, so this spike answers from the
repo's own integration surface instead of a live trial.

## Evaluation

What an external executor needs from Agentgrid (proven by the Zeroshot
integration, ADR 0002):

1. **A spawn contract** — today: wrapper binary (`adapter-<id> --prompt`),
   ACP launcher (`AGENTGRID_ACP_LAUNCH_<ID>`), or the cluster shape
   (`run/kill/export` with `--id/--workdir/--role/--prompt`). A hypothetical
   h5i executor fits the third shape with zero new primitives.
2. **Resource ceilings** — `ResourceLimits` in `SpawnRequest` + backend
   `enforced_limits` reporting + `BackendOutcome::ResourceLimit →
   error_code=resource_limit` (ADR 0003). Any executor that cannot prove a
   ceiling is honestly reported as non-enforcing; strict profiles refuse it.
3. **Provenance carriage** — `ProvenanceRecord {originator, external_id,
   label}` already rides `Assignment` → node → `CompleteAttemptRequest` →
   the `attempts.provenance` column (Stage 13). An h5i run would set
   `originator = "h5i"`, `external_id = <h5i execution id>` via
   `AGENTGRID_PROVENANCE_ORIGINATOR/EXTERNAL_ID/LABEL` — no schema change.
4. **Capability honesty** — probe pair (binary + runtime) with version pin,
   fail-closed `ready=false`, degraded nodes excluded from scheduling.

Nothing in 1–4 is h5i-specific: the integration surface is executor-agnostic
by construction.

## Decision

- **No-go for bundling/vendor­ing anything h5i-shaped now**: without a binary
  there is nothing to conformance-test, and the repo does not ship speculative
  drivers (ponytail rules). The Zeroshot driver is the template: when an h5i
  binary lands, the integration is a small driver + stub + E2E, no core
  changes.
- **Go for the contract**: provenance, limits, and capability probing already
  cover what h5i would need. No code changed in this spike (docs only).

## Revisit when

An h5i binary (or a frozen CLI contract) is available: implement
`node-daemon/src/h5i.rs` mirroring `zeroshot.rs`, a stub E2E, and set
`ProvenanceRecord.originator = "h5i"` on its completions.
