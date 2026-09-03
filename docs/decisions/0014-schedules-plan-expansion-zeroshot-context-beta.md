# ADR 0014: Schedules, plan expansion, zeroshot, shared context — beta contract

Status: accepted (2026-09, v0.5 line).

## Context

After ADR 0013 froze workflows and the skills registry, four experimental
areas still carried the "may change at any time" warning in the README
maturity table. Each has a real consumer in the repo now and its semantics
stayed unchanged across several releases; the label has stopped reflecting
reality and started blocking downstream work ("don't build on this").

This ADR lifts the remaining experimental row to **beta** with the same
additive-compatible rule as ADR 0013 and pins their exact public shape.

## Contract (beta)

### Schedules (`/v1/workflows/{id}/schedules`, + run auto-firing)

- `interval_seconds >= 1`; a tick spawns at most one `WorkflowRun` per
  schedule (overlapping ticks coalesce; covered by the
  `overlapping_schedule_ticks_fire_exactly_one_run` API test).
- `autonomy: l0..l4` is one of the workflow autonomy levels; `l4`
  additionally requires the template to declare a `WorkflowBudget`:
  the L4 gate (`ratify_l4_schedule`) is behavioral config, not a secret.
- `enabled: false` pauses the schedule without dropping last-run state.
- Schedule CRUD routes stay as-is; additive fields only.

### Step plan expansion

- An `expandable` architect step may return one or more
  `\\\`\`\`plan` fenced YAML blocks listing `PlanStep` objects
  (`id`, `prompt`, `depends_on`, `role`, optional `adapter` /
  `requested_node_id` / `retryable` / `max_attempts`). Unknown fields are
  ignored.
- At acceptance time the architect step is recorded as succeeded and the
  inserted worker steps become the run's remaining DAG; no re-expansion of
  the architect step itself happens (single level).
- Cases where no plan fences are emitted: no expansion, run stays as
  authored. (**Not** a failure mode.)

### `zeroshot` cluster adapter

- Built-in cluster adapter backed by a containerized codex executor.
  The node advertises `zeroshot` capability only when the executor probe
  (docker presence + image reachability) passes; operator off via absence
  of capability cuts scheduling before task start.
- Conductor↔executor↔verifier driver semantics live in `crates/common/src/
  cluster.rs`; the public surface contract is: task adapter name is
  `zeroshot`, attempts may result in `zeroshot_unavailable` on nodes where
  the capability probe failed.

### Shared task-group context (`/v1/task-groups/{id}/context`

- Upsert `PUT /v1/task-groups/{id}/context/{key}`, list
  `GET …/context`, read-one `GET …/context/{key}` (404 semantics),
  `DELETE …/context/{key}`.
- Payloads are opaque UTF-8 (no schema). Values are visible to any consumer
  with the group bearer; per-attempt writes are appended, never replaced
  without the write-side seeing the prior value.

## What may still change in beta

- Internal structural layout (sqlite columns, lease rows).
- Error message text.
- Optional extra fields that consumers must tolerate (serde
  `#[serde(default)]` on read).
- The zeroshot executor image id (operator-owned config, not a contract).

## Out of scope (not promised in beta)

- Cross-release snapshot stability of the schedule tick history
  (`last_run_at` is "informational").
- Freeze of the zeroshot container base image.
- A generic "context store" type beyond the `task-groups` key-value map.
