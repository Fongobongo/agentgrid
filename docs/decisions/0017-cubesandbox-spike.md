# ADR 0017 (spike): CubeSandbox strong-isolation profile — go/no-go

Status: spike evaluated (Stage 12) — **no-go for a new backend, go for the
Secure profile as the shippable composition**

## Context

Stage 12 asks for a CubeSandbox spike: a "strong isolation" execution profile
beyond what the docker/systemd backends enforce. No CubeSandbox binary or
spec was available to evaluate here, so this spike compares the requirement
against what the repo already enforces.

## What "strong" would have to add

Over the current docker sandbox (`--cap-drop=ALL`,
`--security-opt=no-new-privileges`, `--network none`, optional `--read-only`
+ tmpfs, `--pids-limit/--memory/--cpus`, digest-pinned image, per-attempt
uid via `--user`) and the systemd-scope backend
(`MemoryMax/CPUQuota/TasksMax` in the daemon's user manager), a stronger
profile would need at least two of:

1. **Syscall filtering** (seccomp-bpf allowlist) — docker's default seccomp
   profile already applies inside our containers; a custom allowlist would be
   per-agent tuning, not a general backend.
2. **User namespaces** (`--userns-remap`) — host-daemon config, not per-attempt
   argv; an operator knob, not a backend.
3. **No host-user overlap at all** — already available via
   `AGENTGRID_RUN_AS_USER` (Plan 6.11) composed with the sandbox.

All three are operator configuration over the existing backends, not a new
backend trait: the `SandboxKind` seam (`sandbox_prefix`/`sandbox_command`)
already carries every knob a stronger profile needs.

## Decision

- **No-go for a separate CubeSandbox backend**: it would be a fourth spelling
  of knobs the docker/systemd paths already own, with no binary to prove the
  difference. If a CubeSandbox runtime appears, it slots in as a new
  `SandboxKind` variant behind the same prefix builders + the backend
  conformance suite (`crates/adapters/tests/conformance.rs`).
- **Go for the Secure profile (ADR 0015)** as the shippable strong posture:
  isolated backend + strict task gate + autonomy ≤ l2 + uid separation. No
  code changed in this spike (docs only).

## Revisit when

A CubeSandbox runtime with a demonstrable gap (e.g. per-agent seccomp
allowlist generation) is available: add the variant, extend the conformance
fixtures, and re-run the two-backend workflow E2E with it as the isolated leg.
