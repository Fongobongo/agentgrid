# ADR 0013: Workflows and skills registry — beta contract

Status: accepted (2026-09, v0.5 line).

## Context

Workflows (DAG engine, schedules, run projection) and the skills/MCP
registry have been labeled **experimental** since Stage 7/13 with the README
warning "semantics may change". Meanwhile the roadmap now treats them as
load-bearing: `run-real-agent.sh` and workflow E2E tests exercise them on
every push, and downstream tooling (CI smoke, `ag run-pipeline`) already
relies on the current response shapes. Keeping them "experimental" forever
is the worst option — users can't tell what's safe to build on, while the
project quietly owes stability anyway.

This ADR freezes the current surface as the **beta contract**:
source-compatible evolution only (additive JSON fields, new enum variants
sent as snake_case strings, new routes) until the beta label drops, and no
silent in-place redefinition of existing fields.

## Contract (beta)

### Types (agentgrid-common)

Frozen shapes, additive-compatible only:

- `WorkflowStep`, `WorkflowTemplate`, `WorkflowRun`, `WorkflowStepRun`,
  `WorkflowRunStatus`, `WorkflowStepStatus`, `WorkflowRole`
  (`architect | worker | reviewer | integrator | verifier`),
  `WorkflowBudget` / `BudgetUsage` / `BudgetSnapshot` / `BudgetBreach`.
- Skills / MCP: the `Skill` and registry DTOs in `agentgrid_common`
  (skill id, version key, curated bundle path, `enabled_pair`), plus
  `AdapterSpec` fields that reference pinned skills.

New versions of dependency payloads (e.g. a step `depends_on`) must stay
compatible with older CPs: unknown keys are ignored, missing keys keep
their `#[serde(default)]` semantics (this is already how the common types
are written and now becomes a hard rule — tests in
`crates/common`/`store/tests.rs` pin it).

### Routes (control plane, `/v1`)

Stable from beta on; changes are additive:

- `POST /v1/workflows` — create template; rejects cyclic/invalid DAGs with
  a machine-readable error (`invalid_dag`), duplicated `step_id`, or
  unknown `role`.
- `GET /v1/workflows`, `GET /v1/workflows/{id}` — template listing/show.
- `POST /v1/workflows/{id}/runs` — start a run (`CreateWorkflowRunRequest`).
- `GET /v1/workflow-runs/{id}` — `WorkflowRunWithSteps` (run + step
  instances).
- `GET /v1/workflow-runs?...` — keyset pagination (cursor cursor-encoded,
  filters `status`, `workflow_id`). Cursor format is opaque and may change
  between releases; **this is the one place clients must not persist
  cursors across releases** until GA.
- `POST /v1/workflows/{id}/schedules` — interval schedule with autonomy
  gate; the L4-gate rule (budget must exist on the template) is stable.
- `GET /v1/workflows/{id}/schedules` and schedule enable/disable.
- `GET /v1/workflow-runs/{id}/projection` — live `WorkflowProjection`
  (roles, verdicts, budgets) for UI/CLI streaming.
- `POST /v1/skills`, `GET /v1/skills`, skill delete/publish/attach deltas
  covered by `Skill` DTO updates only (additive).

### Semantics that are now fixed

1. **DAG**: strict acyclic at create-time; validation rejects cycles,
   unknown step references, and missing planner-role steps on expandable
   templates (ADR 0004). Frozen semantics — no implicit implicit expansion
   on `runs` other than the Stage 13 `plan` re-expansion.
2. **Run execution**: one `Task` per step instance; worker role runs on the
   declared `adapter` for the task; step enters `succeeded` once the task
   is terminal-succeeded and its verdict is recorded.
3. **Schedule**: `interval_seconds >= 1`; at each tick the schedule fires
   exactly one run (overlapping ticks coalesce); `autonomy` defaults to
   `l2`; `l4` requires `WorkflowBudget` present on the template
   (`ratify_l4_schedule`).
4. **Projection**: `RoleRunStatus` maps to `plan | work | review |
   integrate | verify | done | failed`; clients should render from
   `WorkflowProjection`, not by diffing raw steps.
5. **Skills**: server is source of truth for curated bundles; nodes sync
   by hash and *only* install bit-identical bundles matching the server
   hash (trust pinned by `skills_trust`).

### What may still change in beta

- The step-instance table layout (internal, not part of the contract).
- The exact text of validation/rejection messages (the *code* stays).
- Workflow template export/import file format.
- Cursor encoding for keyset pagination.
- Additional `WorkflowRole` variants may be added (additive enum); clients
  must tolerate unknown variants.

### Out of scope (stable promises we are not making yet)

- Cross-CP workflow migration.
- Workflow versioning beyond single-template history.
- A public programmable hook API inside the run loop.
- Backwards-compatible `agentgrid-gateway` ACP exposure of workflows
  (gateway stays prototype per ADR 0007).

## Consequences

- README maturity table moves *workflows* and *skills/MCP registry* from
  `experimental` to `beta`; `schedules / plan expansion / zeroshot /
  context provider` keep their label until their own micro-ADRs land.
- CI and E2E contract tests must pin the frozen shapes. Adding or
  renaming a route/DTO field in the frozen set requires a matching
  CHANGELOG entry and a compatibility shim unless it is purely additive.
- Upgrade path: no DB migration needed beyond existing migrations; the
  change is a documented contract, not a code migration.
