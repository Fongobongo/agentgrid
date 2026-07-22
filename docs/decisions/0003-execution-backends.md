# ADR 0003: Execution backends — capability-honest resource limits

Status: proposed (Stage 12)

## Context

Agentgrid supports multiple execution backends for an attempt's agent process:
the native `ProcessBackend` (subprocess in a worktree, no isolation), a future
container backend (Docker/Podman), and an OS-level backend (Linux cgroups v2 /
systemd transient scope). Stage 12 of the plan asks for: a conformance suite,
resource limits, and a `resource_limit` error_code when a ceiling is hit.

The hard correctness property is **capability honesty**: an effort to apply a
strict profile must not silently run on a backend that cannot enforce its
limits. A `MemoryMax=512M` that the backend ignores is worse than no limit at
all — it lets an OOM eat the node's memory while the operator believes the run
is bounded.

## Decision

1. **Resource limits are part of `SpawnRequest`**, not backend config. A profile
   that needs isolation attaches `ResourceLimits { memory_max, cpu_quota_pct,
   tasks_max }`; the backend applies what it can.

2. **A backend reports what it enforced.** `BackendProcess::enforced_limits`
   is `false` for `ProcessBackend` (no cgroup), `true` for a cgroup/container
   backend. The node surfaces this in the attempt's event stream so the operator
   sees the actual isolation level, and profiles can refuse a strict run on a
   backend that reports `false` (Stage 12 secure profile).

3. **`BackendOutcome` is the exit classification.** `classify_exit` maps a raw
   `ExitStatus` → `Exited`/`Killed`; a cgroup-aware backend upgrades a
   `Killed(SIGKILL)` to `ResourceLimit{reason}` when the cgroup reports an OOM.
   `BackendOutcome::error_code()` yields `resource_limit:<reason>` so the
   control-plane flows the same error class as `timeout` / `validation_failed`.

4. **Linux backend = cgroups v2 or a systemd transient scope.** systemd
   `systemd-run --scope --uid --gid -p MemoryMax=512M -p CPUQuota=50% -p
   TasksMax=128` is the preferred path (delegable, survives the daemon, no
   manual cgroup bookkeeping); direct cgroup-v2 writes are the fallback when
   systemd is absent. macOS uses process groups + documented limits (no hard
   memory ceiling); Windows uses Job Objects. Each backend's
   `available()/enforced_limits` honestly reflects what that platform can do.

5. **Container backends re-use the same `ResourceLimits`.** A Docker/Podman
   adapter maps the limits to `--memory`/`--cpus`/`--pids-limit` and reports
   `enforced_limits=true`. A Zeroshot cluster (ADR 0002) is itself a kind of
   execution backend, so the same `ResourceLimits` + `error_code=resource_limit`
   apply to the cluster.

## Consequences

- A strict/unattended profile (Gate D) refuses to start on `ProcessBackend` —
  it asks for limits that backend cannot enforce. This is the same fail-closed
  discipline as the wrapper-adapter boundary (Stage 9.1) and the Zeroshot
  capability probe (Stage 10).
- The conformance suite (existing `tests/conformance.rs`) drives any backend
  through one `spawn` + stream + collect; a new backend passes the same smoke
  the mock adapter does.
- `error_code=resource_limit` is a first-class terminal outcome alongside
  `timeout`/`validation_failed`/`infrastructure_failed`, so the control-plane's
  retry policy can treat it (e.g. never auto-retry an OOM).

## Future

- The concrete cgroup/backend impl lands when a strict profile needs it; the
  contract, error mapping, and conformance hook are in place now.
- Direct-cgroup-v2 vs systemd scope choice is per-node config; not decided here.
