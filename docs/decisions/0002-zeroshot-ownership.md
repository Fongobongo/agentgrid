# ADR 0002: Zeroshot ownership — 1 Agentgrid Attempt = 1 Zeroshot Cluster

Status: proposed (Stage 10 spike)

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

## Future

- A transport that lets one cluster host several Agentgrid attempts (verified
  loop pool) would break the 1:1 invariant; revisit if Zeroshot gains a
  first-class multi-task mode, but until then 1:1 keeps cancel/lost-node
  recovery provable.
- Version pin + probe of the Zeroshot CLI is a separate ADR when the binary lands.
