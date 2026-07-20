# Threat Model — agentgrid 0.1.x

Status: living document. Covers the 0.1.1 hardening release and accumulates
follow-ups per release. Read alongside `AGENTS.md` and the ADRs in
`docs/decisions/`.

## Scope

agentgrid is a control plane (Axum + SQLite, local disk) plus one or more node
daemons that run coding agents (mock/claude/opencode/ACP-native) in per-attempt
git worktrees. Operators act via CLI, web UI, or a chat gateway (Telegram).
External ACP clients can drive the grid as an ACP agent (Stage 6) since 0.2.

## Assets

- **Secrets**: adapter API keys (`AGENTGRID_ADAPTER_ENV`), `AGENTGRID_SECRETS`
  (masked in logs), enrollment tokens, user passwords (argon2-hashed), the JWT
  signing secret.
- **Integrity of state**: SQLite task/attempt/workflow/approval records, git
  repos cloned onto nodes, produced commits/patches, review artifacts.
- **Availability**: the control plane and node daemons must not be wedged by a
  bad adapter, a runaway agent, or a compromised node.

## Trust boundaries

1. **Operator → control plane** (user routes `/v1/*`, web UI). Auth = JWT
   (HttpOnly + SameSite=Strict cookie for the browser since 0.1.1; `Bearer`
   for CLI/gateway). Brute-force limited on `/v1/auth/login`; user-enumeration
   avoided (generic errors).
2. **Node → control plane** (`/v1/node/*`). Auth = enrollment-token → long-lived
   credential (hash). Nodes are semi-trusted: they execute agents, so a
   compromised node can tamper with its own attempts — but cannot forge other
   nodes' attempts or read secrets it was not handed.
3. **Control plane → adapter process** (on the node). The adapter runs in a per-
   attempt worktree; it is least-trusted (arbitrary code execution by design).
   Secrets are forwarded via env, masked in streamed output and artifacts.
4. **ACP client → grid** (`agentgrid acp-agent`, Stage 6). External clients are
   untrusted; they can create/cancel tasks they own and answer approvals routed
   to them, but cannot enumerate or touch other clients' tasks.

## Threats and mitigations

| # | Threat | Mitigation | Gap / follow-up |
|---|--------|-----------|-----------------|
| T1 | Secret in agent stdout/stderr leaks into events, artifacts, commit, patch | `AGENTGRID_SECRETS` masked in all stream paths, `validation.log`, `changes.patch`; agent logs excluded from the commit/diff via per-worktree `.git/info/exclude` (0.1.1) | binary-safe artifact API + `openat`/`O_NOFOLLOW` still follow-up |
| T2 | Crafted artifact name reads/writes outside the artifact root (`../../etc/passwd`) | upload + download both gate on `is_safe_artifact_name`; `Store::artifact_path` canonicalizes and checks the resolved dir stays under the root (0.1.1) | descriptor-relative write API pending |
| T3 | Shell injection via repo/branch/URL/adapters | git invoked via `Command::arg` (no `sh -c`); tokens validated (`[a-z0-9-_]`, no traversal) | `checkout -B` on shared clone → bare mirror follow-up |
| T4 | Two parallel attempts of one repo race the shared clone | per-repository in-process `Mutex` across fetch/`checkout -B`/`worktree add` (0.1.1) | cross-process file lock follow-up |
| T5 | Node vanishes mid-attempt → wedged/`running` tasks | node offline → non-terminal attempts atomically `lost`; lease + ack deadline revert | — |
| T6 | Event/completion loss on node kill / network blip | durable JSONL outbox; records removed only after CP ack; startup redelivers pending completions (0.1.1) | RAM/spool size limits + `output_truncated` + artifacts-in-spool pending; E2E pending |
| T7 | Web JWT stolen via XSS (localStorage) | HttpOnly + SameSite=Strict cookie; no token in JS (`0.1.1`); `Secure` under `AGENTGRID_COOKIE_SECURE=1` | CSRF limited to SameSite=Strict; per-request Origin check not added |
| T8 | Brute-force / user-enumeration on login | sliding-window rate limit (fail-closed 429); generic errors | — |
| T9 | Weak/rotating JWT secret | requires stable `AGENTGRID_JWT_SECRET` (warn/fail) | — |
| T10 | Accidental data loss / unreplayable state | `VACUUM INTO` backup; WAL `TRUNCATE` checkpoint on graceful shutdown + periodic; `quick_check` at boot | rolling N/N-1 only where declared |
| T11 | Compromised/untrusted project skill auto-activated from a cloned repo | skill trust gate: project skills don't activate without explicit trust | per-skill trust management UI/CLI pending |
| T12 | Dangerous agent command (package install, destructive) executed unattended | command-policy provider (`ask`/`deny` default), audit per decision; autonomy L0–L4; fail-closed on provider error | pluggable providers + ACP/adapter enforcement boundary noted as not-yet-strict for wrapper-only adapters |
| T13 | Docker-mount secrets leak (sandboxing/backends) | not yet enforced (sandbox optional, off-by-default) | h5i/CubeSandbox spikes + secure profile (Stage 12) |
| T14 | Incompatible/rogue node joins a grid | protocol versioning; incompatible node marked `degraded(incompatible_protocol)` | — |

## Operational safety invariants

- The task state machine is pure; an attempt never reports a false `succeeded`
  (success requires clean exit AND no distinct failure category; validation
  failure with agent exit 0 → `failed/validation_failed`).
- Cancellation is idempotent and the `cancelled` outcome holds regardless of the
  agent exit code.
- Approvals fail closed (`ask`/`deny` default); an unattended run cannot widen
  autonomy without an explicit policy + budget.
- Disk low → node self-marks `degraded` (< `AGENTGRID_DISK_LOW_MB`).

## Out of scope for 0.1.1

- Strong per-adapter command interception (needs structured tool calls — ACP or
  a backend policy), so wrapper-only adapters are not claimed "strict".
- Network/FS isolation beyond the optional Docker sandbox profile.
- Multi-region / shared-nothing CP failover (single active instance by design).
