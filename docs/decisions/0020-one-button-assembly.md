# ADR 0020: One-button assembly — goal in, merged project out

Status: accepted

## Context

Project assembly (ADR 0019) wired every stage but still made the operator
assemble the assembly: write a template (or an architect step), create the
run, approve the plan, pass GitHub flags per step. The requested entry is
one button: free-text goal → architect-planned multi-host build with
collection and optional publish.

## Decision

1. **`POST /v1/assembly`** (`AssemblyRequest`: goal, repository?, adapter?,
   base_commit?, context?, auto_approve_plan=false, budget?, github_*).
   The server validates (empty goal / oversized goal / bad GitHub target →
   400/413, same shapes as task create), builds an ephemeral template (one
   expandable architect step carrying the composed goal prompt), starts the
   run marked `assembly` with the requested adapter, and returns it with an
   audit event. Admin-only by the existing middleware — an unattended run
   spends agent budget and can push to GitHub.
2. **Architect prompt is composed server-side** (`assembly_architect_prompt`,
   pure + unit-tested): goal + the machine-plan contract (`parse_plan_steps`
   shape: YAML array of id/prompt/depends_on/role/adapter) + the rule that
   collection is appended by the system (the agent must not add a merge
   step). The plan still parses fail-closed; garbage plans keep the run
   `PlanReady`.
3. **Approval appends the collector** (same txn as expansion + CAS flip):
   assembly runs whose plan lacks a collector get one depending on every
   expanded step; an explicit plan collector wins untouched. Plan steps
   without an adapter inherit the run default (legacy runs: NULL → `"mock"`
   at activation, byte-identical behavior).
4. **Autopilot guardrails**: `auto_approve_plan` without an explicit budget
   gets a conservative default template budget (50 rounds, 4h wall) — an
   unattended run cannot iterate forever. Manual assembly gets none (the
   human gates). Every auto-expansion audits (`workflow.plan_auto_approved`).
5. **Entries**: `ag assemble <goal> [--repo/--adapter/--auto-approve/
   --github-*]` (CLI), `/assemble <adapter> <goal...>` (Telegram gateway:
   trusted allowlist, auto-approved, merge-only, 30-min spawned wait with a
   step-verdict summary), `#/assemble` web form (goal + adapter + repo +
   auto-approve + GitHub section, redirects to the run page). No new auth
   model anywhere: CLI/web ride user JWT (admin for create), Telegram rides
   its allowlist.

## Consequences

- Raw goal text becomes an architect-led assembly run in one call; every
  powerful default (auto-approve, publish, budget) is explicit at the call
  site and visible in events/audit.
- New remotes stay private-by-default (ADR 0019); the gateway path cannot
  publish at all (merge + artifacts + chat report only).
- Proof: `tests/e2e/run-autoassembly.sh` (plan-stub architect speaking the
  wrapper plan-event contract — no LLM — through auto-approve, workers, and
  the system collector; CI), store tests (flag/adapter/architect, collector
  append/skip, adapter fill), API test (201 + shape + 400/413), gateway
  format test, web vitest + build.

## Non-goals / follow-ups

- The architect still needs a real planning-capable adapter for real goals
  (mock cannot plan; the stub proves mechanics only).
- No cross-run learning (which decompositions worked) — future mailbox/loops
  work, not this ADR.
