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
| T2 | Crafted artifact name reads/writes outside the artifact root (`../../etc/passwd`) | upload + download both gate on `is_safe_artifact_name` (validated before the ownership check so a traversal attempt never discloses attempt existence); `Store::artifact_path` canonicalizes and checks the resolved dir stays under the root; NUL/backslash/percent-traversal all rejected (0.1.1, expanded 0.4.1) | descriptor-relative write API (`openat`/`O_NOFOLLOW`) + symlinked attempt-dir hardening pending |
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
| T16 | Open bootstrap window: before the first user exists unauthenticated callers create tasks/repos/tokens | user routes fail **closed** (`503`) while `user_count() == 0`; only `/v1/auth/setup`, `/health/*`, and static UI are reachable; first user requires a one-time 15-min setup token (printed to stdout) consumed on first use; env bootstrap backdoor removed (0.4.1) | — |
| T17 | Auth falls open on transient DB error (confused-deputy: outage mistaken for "no users → public") | `require_user_auth` returns `503` (never allows) on any DB error instead of treating it as "no users" and falling through (0.4.1) | — |
| T19 | Artifact integrity: a node uploads a tampered artifact or lies about its hash; a corrupter propagates into downstream workflow steps | server computes SHA-256 of every uploaded byte stream; a caller-supplied hash hint that disagrees is rejected with `422` and the artifact is not published; only the server-computed hash is persisted (0.4.1) | — |
| T20 | Stored XSS via artifact download (HTML/SVG/JS served inline from a trusted origin with session cookies) | artifact downloads downgrade HTML/SVG/JS/XML/unknown types to `application/octet-stream`, add `Content-Disposition: attachment` (RFC 6266 safe filename) and `X-Content-Type-Options: nosniff`; a small inline-safe allowlist covers images/text/archives only (0.4.1) | separate artifact origin (no session cookies) + CSP follow-up |
| T21 | Crash between artifact write and metadata commit leaves a half-written published artifact on disk | artifact writes are atomic: raw bytes go to `*.tmp.upload` then `rename` before the DB row is upserted (0.4.1) | — |
| T22 | Static file traversal: `/../`, `%2e%2e/`, backslash, or absolute paths in the web UI fallback read files outside `web_root` (lexically-flawed `starts_with`) | the SPA fallback rebuilds the target path from a path-component whitelist (`Component::Normal` only; `ParentDir`/`RootDir`/prefix rejected), canonicalizes the web root once per request and re-checks the canonicalized file path stays under it — symlinks escaping the root are blocked (403) (0.4.1) | — |
| T23 | SPA fallback served stale after deploy / under-cached hashed assets | hashed assets under `assets/` get `Cache-Control: public, max-age=31536000, immutable`; `index.html` and non-hashed files get `no-cache` so deployments take effect without a hard refresh (0.4.1) | — |
| T26 | Lease/ACK race conditions: a concurrent `ack_attempt` and a `revert_expired_leases` sweep (or an `offline` sweep vs a fresh heartbeat) could double-flip an assignment or bob decrement `active_attempts` (concurrency counter drift, lost/duplicate attempts) | `ack_attempt`, `revert_expired_leases`, `mark_offline_nodes`, `mark_node_offline`, and `revoke_node` now run under a single write transaction with compare-and-set `WHERE status = ...` guards and `rows_affected()` checks; the task is only flipped when `assigned_attempt_id` matches; `active_attempts` is decremented only for attempts the CAS actually moved (0.4.2) | optimistic `version` guards for tasks/attempts; explicit cancel↔complete, lost↔complete, retry↔late-completion race tests; high-iteration concurrent invariant test follow-up |
| T25 | Node bootstrap over SSH accepts an unknown/tofu-switched host key (MITM hijack of the install + enrollment token) and runs the node daemon as root with the token left on disk | `ag nodes install` now fails CLOSED on an unknown SSH host key (`StrictHostKeyChecking=yes` by default), exposes `--accept-new-host-key` and a pinning `--host-key-fingerprint`, and never auto-sets `AGENTGRID_ALLOW_ROOT=1` (opt-in `--allow-root`). The daemon scrubs `AGENTGRID_ENROLL_TOKEN` from its `AGENTGRID_ENV_FILE` after persisting `credential.json` (atomic 0600 rewrite), and `deploy/install-node.sh` keeps the token in its own `0600` `EnvironmentFile=` + adds the full systemd hardening directive set (0.4.1) | remote user provisioning + systemd-via-CLI install + adapter binary checksum/verify + rollback + idempotent reinstall + `ag node uninstall` follow-up |
| T24 | Unattended adapter defaults to a dangerous permission bypass (`--dangerously-skip-permissions` / `--auto`) so an unguarded agent auto-runs destructive tools | Claude `--dangerously-skip-permissions` and opencode `--auto` are now gated behind a single operator opt-in `AGENTGRID_UNSAFE_UNATTENDED=1` (default off); the adapter loudly warns on stderr when on, and never adds the dangerous flag otherwise (0.4.1) | sandbox=none × unsafe gating, structured permission interception (`permission_interception` capability), strict-profile refusal of wrapper adapters, provenance record and UI badge follow-up |
| T18 | Predictable weak bootstrap credentials in deploy defaults | production `docker-compose.yml` has no `admin/changeme`/`dev-insecure-secret`; `deploy/compose/up.sh` generates a random JWT secret + admin password and completes bootstrap via the setup token; `install-control-plane.sh` no longer bakes `admin/changeme` into the systemd unit (0.4.1) | demo compose keeps insecure static defaults for local hacking only (documented) |
| T15 | A node mutates or reads another node's attempt (cross-node isolation breach) | every `/v1/node/attempts/*` mutation (events, complete, ack, cancel-poll, session, artifact upload) and the node-scoped artifact read enforce authenticated-node ownership of the target attempt; a foreign attempt yields `403` (or `404` to hide existence); upstream artifact reads are restricted to the consumer node whose workflow step depends on the producer task (`can_node_read_upstream_artifact`); revoked nodes fail at `require_node_auth` (0.4.1) | every node mutation also checks a per-attempt fencing token (`X-AgentGrid-Fencing-Token`); a stale token from a superseded attempt/lease yields `409` so a restarted node cannot mutate an attempt the scheduler already re-assigned (0.4.2, implemented) |

## Operational safety invariants

- The task state machine is pure; an attempt never reports a false `succeeded`
  (success requires clean exit AND no distinct failure category; validation
  failure with agent exit 0 → `failed/validation_failed`).
- Cancellation is idempotent and the `cancelled` outcome holds regardless of the
  agent exit code.
- Approvals fail closed (`ask`/`deny` default); an unattended run cannot widen
  autonomy without an explicit policy + budget.
- Disk low → node self-marks `degraded` (< `AGENTGRID_DISK_LOW_MB`).
- **Fencing tokens (0.4.2):** every attempt mutation (events/ack/complete/artifact/session) must carry the per-attempt `X-AgentGrid-Fencing-Token` minted at assignment. A token that does not match the attempt's current fencing token yields `409 Conflict` and mutates nothing, so a stale writer — a node daemon that restarted after its lease expired, or two nodes racing for the same attempt — cannot corrupt an attempt the scheduler has already superseded. The token rotates on each re-assignment.
- **SSH credential policy (0.4.2):** git over SSH must use a dedicated deployment key the operator provisions, not the daemon user's interactive agent.
  - Run the node daemon under a dedicated system user with a minimal `~/.ssh/config` (deploy keys only). Do **not** forward `SSH_AUTH_SOCK` into the daemon environment — unset it in the unit file so a logged-in operator's agent is never used by a clone/fetch.
  - Prefer `git@` (scp) or `ssh://` URLs over `file://` for remote repos; `file://` is permitted only for trusted local test repos (the `validate_git_url` scheme allowlist rejects `javascript:`/`data:`/`ftp:`/arbitrary schemes).
  - Pin repository hosts with a known `~/.ssh/known_hosts` (no `StrictHostKeyChecking=no`); the node install path fails closed on an unknown host key by default.
  - A deploy key grants only repo clone/fetch; never place a key that can push unless a workflow explicitly requires upstream push (none does in 0.4.2).

## Hardening pass additions (0.4.x)

- **Daemon-env isolation:** the adapter subprocess no longer inherits the node
  daemon's environment (`env_clear()` + `PATH`/`HOME` + the explicit
  `AGENTGRID_ADAPTER_ENV` allowlist + profile-declared secrets). Node
  credentials, API keys and `AGENTGRID_SECRETS` never reach the agent. (P1 27)
- **Secret redaction variants:** `mask_secrets` masks base64- and
  percent-encoded forms of each configured secret, so a diagnostic that
  printed an encoded secret cannot leak the raw value. (P1 27)
- **Global event cursor (SSE ordering):** events carry a monotonic `ingest_id`;
  SSE `id:`/`Last-Event-ID` resume on the global cursor, so a client that
  reconnects across attempts never reorders or duplicates events. (P0 9)
- **Node drain:** a drained node stops receiving NEW assignments while its
  in-flight attempts finish (heartbeat stays online) — a maintenance path that
  cannot strand work. (P2 37)
- **Artifact integrity:** artifact downloads return `X-Artifact-Sha256` (the
  server-computed hash) so consumers can verify what they received. (P2 36)
- **DB integrity:** FKs (attempts→tasks/nodes, task_events/artifacts→attempts,
  node_repositories→nodes/repositories, approvals→tasks) + status CHECKs
  backstop NEW writes; orphan rows from pre-FK databases are surfaced by
  `count_orphan_rows`/`storage_gc`. (P1 21)
- **Agent env allowlist for profiles:** profile-declared secrets are forwarded
  to the child explicitly (not inherited), and `AGENTGRID_SYSTEM_PROMPT` is the
  only other forwarded hint. (P1 27)

## Out of scope for 0.1.1

- Strong per-adapter command interception (needs structured tool calls — ACP or
  a backend policy), so wrapper-only adapters are not claimed "strict".
- Per-connection egress audit and per-CIDR egress filtering (docker CLI cannot
  express them; see ADR 0005 — `restricted` maps to `--network none` and
  `allowlist:` refuses to start, fail-closed, until an egress proxy lands).
- Multi-region / shared-nothing CP failover (single active instance by design).

## 0.5 execution isolation additions (ADR 0005)

- **Orphan containers:** containers are stamped `--label agentgrid.node=<id>`
  and removed at daemon startup, so a SIGKILLed daemon's stranded containers
  are reclaimed. (P1 633)
- **Sandbox smoke test:** adapters are probed inside the image
  (`command -v <bin>`) at startup; a missing in-image adapter marks the node
  degraded. (P1 637)
- **Runtime probe:** `docker version --format {{.Server.Version}}` at startup
  when the sandbox is configured; an unreachable runtime is loud, not silent.
  (P1 617)
- **Artifact mount:** `AGENTGRID_SANDBOX_ARTIFACT_DIR` mounts a writable
  `/artifacts` independent of `--read-only`, so a read-only worktree still
  has a writable output place. (P1 630)
- **Honest capability report:** `enforced_limits` is true only when Docker +
  resource limits + effective network `none` all hold; a bridge/unrestricted
  node reports false rather than claiming isolation it does not apply.
  (P1 960)
