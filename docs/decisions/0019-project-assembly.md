# ADR 0019: Project assembly — from a text task to a pushed repo

Status: accepted

## Context

The requested operator loop: a text task is decomposed, runs on several
hosts, the parts are merged into one project, saved, published to a (possibly
brand-new) remote repository, and reported back. All the machinery existed
except the last mile (remote creation, from-scratch assembly, unattended
plan approval). This ADR records how the pieces compose.

## Decision

1. **Decompose** — unchanged: an `expandable` architect emits a machine plan,
   the run pauses `PlanReady`, a human approves (`POST
   /v1/workflow-runs/{id}/approve-plan`, operator RBAC). **Autopilot
   option:** runs created with `auto_approve_plan: true` (`ag workflows run
   --auto-approve`) expand a parseable plan on the next tick instead of
   waiting; unparseable plans stay `PlanReady` with a warning, and an audit
   event (`workflow.plan_auto_approved`, actor `system`) records every
   auto-expansion. Default off.
2. **Distribute** — unchanged: per-step `requested_node_id`, scheduler
   eligibility, patch-bundle fallback without a shared remote.
3. **Collect** — new `Collector` workflow role: same upstream mechanics as
   `Integrator` (cherry-pick reachable SHAs, `git apply --3way` patch
   bundles otherwise), but works on plain-dir tasks too — the node
   `git init`s the worktree first, so there is a repo to merge into. A
   failing collector `Blocked`s the run for human/repair like an
   integrator, never failing it outright. Only collector steps inherit the
   run's GitHub target.
4. **Publish** — task/run flags `github_push` + `github_repo` (+ `github_issue`):
   unchanged push → PR → issue-comment flow, plus `github_create`: the node
   `GET`s the remote first — 404 creates it **private, no auto-init**
   (`POST /user/repos` or `/orgs/{owner}/repos` by owner kind), anything
   ambiguous fails closed. A just-created remote skips the PR (no base
   branch exists yet) with a `github_pr_skipped` event; the issue comment
   then points at the pushed branch. Malformed targets 400 at create time
   (task and run), never a `succeeded` task with only a push-failed log.
   No token → the long-standing best-effort skip with an event.
5. **Report** — unchanged channels (result events, artifacts, Telegram,
   ACP streaming) plus the new write-back events (`github_repo_exists`,
   `github_repo_created`, `github_pr_skipped`, …).

## Consequences

- The full text → project → push loop runs unattended when the operator
  opts in (`--auto-approve` + `github_create`); every opt-in is explicit,
  audited, and fail-closed by default.
- New remotes are private by default; visibility is the operator's explicit
  flip on GitHub, never an automation default.
- Proof: `tests/e2e/run-assemble.sh` (two workers on two nodes + collector
  merge into one commit, CI), node unit tests for the API client (dummy
  server) and scaffold init, store test for auto-approve, API test for the
  400s. A live-GitHub run is one token away
  (`AGENTGRID_GITHUB_TOKEN=…`, run-level `github_push/github_repo/github_create`).

## Operator recipe (autopilot assembly)

```bash
# 1. Template with workers + collector (collector depends on the workers).
# 2. One run: no human pauses, collector publishes to a fresh private repo.
ag workflows run <template> --auto-approve \
  --github-push --github-repo acme/new-project --github-create
# 3. Watch: ag watch / web UI workflows page / projection endpoint.
```
