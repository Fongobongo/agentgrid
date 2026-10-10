# ADR 0002: Zeroshot ownership — 1 Agentgrid Attempt = 1 Zeroshot Cluster

Status: accepted (Stage 10 implemented against the CLI contract below; the
only remaining external is a real `zeroshot` binary — CI runs the conformant
stub `tests/e2e/zeroshot-stub.sh`)

## Context

Zeroshot is an external "verified loop" executor: it spins up a *cluster* of
agent+verifier containers and runs a task to a verified result. Agentgrid wants
to offer it as an **optional** adapter (Stage 10 of the plan, 0.4) — one Agentgrid
task can delegate its single attempt to a Zeroshot verified loop on a chosen
node, with cancel killing the whole loop.

The core question is the ownership lifecycle. Zeroshot clusters are long-lived,
stateful, and expensive; Agentgrid attempts are lease-bound, cancellable, and
restartable. A mismatch in that lifecycle is a correctness/security bug:

- If a cancel doesn't kill the cluster, work keeps running on credentials the
  operator revoked — a runaway.
- If a daemon kill orphans a cluster, no one owns it — resource leak + a
  cluster nobody can kill.
- If a retry reuses a still-running cluster, two attempts race the same
  Zeroshot state.

## Decision

**In invariant: 1 Agentgrid attempt = 1 Zeroshot cluster, 1:1.**

1. **Lifecycle is attempt-scoped.** The Agentgrid attempt owns exactly one
   Zeroshot cluster for its whole live span. A `cluster_id` is recorded on the
   attempt the moment it is created; killing the attempt kills the cluster.
   No shared cluster across attempts, no attempt without a cluster.

2. **Cancel is total.** `cancel_task` / daemon SIGTERM → `kill` the entire
   Zeroshot cluster, not just the local process. A cluster that survives its
   attempt violates the invariant; the node waits (with the existing 10s →
   SIGKILL escalation) for the kill to take before reporting terminal.

3. **Orphan reclaim.** On node startup-reconcile (Stage 11.1), any Zeroshot
   cluster still alive whose `cluster_id` maps to an attempt that is no longer
   `running` on the control plane is killed (the attempt was lost/cancelled
   while the daemon was down). A cluster whose attempt is still `running` is
   re-adopted only by the node that originally owned it (attempt→node affinity
   from the assignment), so a second node never double-adopts.

4. **Retry = new attempt = new cluster.** A failed/cancelled retry creates a
   fresh attempt; the adapter never resumes a dead cluster. Resume semantics
   (Stage 11.5) do not apply across a Zeroshot boundary — the cluster is the
   session, and a dead cluster cannot be resumed.

5. **Results are artifacts.** The cluster's verified output is exported as
   Agentgrid artifacts (patch + a provenance blob); the attempt is terminal
   only after the export. Metadata (cluster_id, executor/verifier roles) ride
   the existing event envelope.

6. **Credentials stay local.** Docker mounts never pass through host
   credentials; the Zeroshot adapter carries only what Agentgrid provisions for
   the attempt (Stage 2.2 secret discipline). This is the same invariant as
   the wrapper-adapter enforcement boundary (Stage 9.1): a Zeroshot cluster is a
   kind of execution backend, so a strict profile pairs it with a backend
   policy (Stage 12).

## Consequences

- An attempt is never short of a cluster and a cluster is never orphaned; the
  1:1 invariant is the contract that makes cancel and lost-node recovery sound.
- Retry is cheap to reason about (new cluster, no state to reconcile), at the
  cost of re-paying Zeroshot's cluster spin-up — acceptable for a verified
  loop where determinism matters more than spin-up latency.
- The adapter contract (`AgentEventEnvelope`) needs no new event type: cluster
  lifecycle maps to the existing attempt events (`status`/`tool_call`/
  `file_change`/`progress`/`result`/`error`); `cluster_id` piggybacks on
  `session_id`.
- Security: cancel-then-die is the failure mode; Docker mount hardening is a
  Stage 12 concern that this ADR defers to.

## Implementation (Stage 10)

Driver: `crates/node-daemon/src/zeroshot.rs`, wired into the wrapper attempt
pipeline (spawn → stream → finalize → validation → artifacts all reused).

**CLI contract** (binary from `AGENTGRID_ZEROSHOT_BIN`, default `zeroshot`):

- `run --id <attempt-id> --workdir <dir> [--role executor|verifier]
  --prompt <prompt>` — blocking; creates (or re-attaches to, same `--id`)
  the cluster and streams adapter-contract lines on stdout
  (`{"type": …, "payload": …}` — `log`/`tool_call`/`file_change`/`result`/
  `error`; unknown lines become raw `log` via the existing stream reader).
  Exit 0 = cluster succeeded. The cluster id IS the attempt id, so retry =
  new cluster holds by construction, and `run` with a live `--id` resumes
  within one attempt (node restart mid-attempt re-attaches; resume *across*
  attempts stays forbidden per §4).
- `kill <attempt-id>` — total kill, idempotent (missing cluster = success).
  The node calls it bounded (15s), best-effort, after every cut-short run
  (cancel/timeout/resource-limit) and every non-zero exit — success keeps
  the cluster so a validation-feedback round can re-attach by `--id`.
- `export <attempt-id> --dir <workdir>` — materialize result files
  (`zeroshot-result.json` + executor output) into the worktree before
  finalize; bounded (60s), best-effort. Uploaded as the
  `zeroshot-result.json` artifact; the normal `changes.patch`/commit flow is
  unchanged.
- Missing binary at attempt time → `infrastructure_failed` with the
  `zeroshot_unavailable` detail (the name 0014 reserves); the capability
  probe (`probe_cluster_adapter`: executor binary + docker runtime + version
  pin) keeps degraded nodes out of scheduling earlier.

**Role mapping:** `Assignment.workflow_role` (resolved from `role_runs` at
assign time, `None` for direct tasks) collapses onto the two sides —
`architect`/`worker`/`integrator` → `--role executor`,
`reviewer`/`verifier` → `--role verifier` (paired with the read-only
worktree the scheduler already enforces for verifiers); direct tasks omit
`--role`. A `cluster_created` status event (cluster id, executor, side)
opens the attempt's event trail.

**Security review (Docker mounts):** the driver argv carries no
`-v/--volume/--mount/--privileged` flags — only `--workdir` travels, and the
only bind is the sandbox worktree mount the wrapper path already applies
(asserted by `run_argv_mounts_nothing_but_the_worktree`). No
`~/.config/agentgrid`, no node credential files, no host paths reach the
cluster. The `run` process inherits the attempt env (LLM keys the agent
needs — identical to the docker-sandbox container env, audit X-N3b); host
credential *files* never travel. Kill/export spawn with `env_clear`.

**Verified profile recipe** (operator-run; secrets are names only, values
stay in node env):

```bash
ag profiles create zeroshot-verified \
  --autonomy l2 \
  --system-prompt "Run the task to a verified result inside the Zeroshot cluster. Do not escape the worktree." \
  --memory-max 2147483648 --cpu-quota 200 --tasks-max 512 \
  --secret-required ANTHROPIC_API_KEY
ag profiles activate zeroshot-verified <rev>
```

E2E: `tests/e2e/run-zeroshot.sh` (stub executor; happy + `fail:3` +
cancel-total, wired into the CI e2e job). A real-binary run is
`AGENTGRID_ZEROSHOT_BIN=/path/to/zeroshot bash tests/e2e/run-zeroshot.sh`
(manual; needs docker + the binary).

**Deferred:** startup orphan-reclaim sweep (§3: `list` + kill of clusters
whose attempts are no longer `running`) — needs a `list` subcommand on the
real binary; the kill-after-cut-short path already prevents new orphans
within one daemon lifetime.

## Future

- A transport that lets one cluster host several Agentgrid attempts (verified
  loop pool) would break the 1:1 invariant; revisit if Zeroshot gains a
  first-class multi-task mode, but until then 1:1 keeps cancel/lost-node
  recovery provable.
- Version pin + probe of the Zeroshot CLI is a separate ADR when the binary lands.
