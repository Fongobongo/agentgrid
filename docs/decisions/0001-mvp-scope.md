# ADR 0001: MVP 0.1 Scope

Status: accepted (2026-07-16)

## Context

agentgrid is a distributed orchestrator for coding agents (Rust). MVP 0.1 is a
control plane + SQLite WAL + node daemon + CLI + web UI, Linux only. These
decisions unblock the start of implementation (Stage 0.1 of the plan).

## Decisions

| # | Topic | Decision |
|---|-------|----------|
| 1 | Project name | `agentgrid`. Binaries: `agentgrid-control-plane`, `agentgrid-node-daemon`, `ag` (CLI), `adapter-mock`. |
| 2 | First real adapter | Deferred to Stage 3. MVP vertical prototype uses the deterministic `adapter-mock` (no LLM). Real adapter (Claude Code / Codex / OpenCode) chosen in Stage 3. |
| 3 | License | MIT. |
| 4 | Interfaces in 0.1 | Both CLI and web UI. Stage 1 builds the CLI; web UI in Stage 4. |
| 5 | Git clone transport | HTTPS / token (recommended). SSH optional later. |
| 6 | Agent auto-commit | Yes — diff + commit saved on attempt completion (Stage 2.5). |
| 7 | Node channel | Long polling (MVP). WebSocket deferred to 0.2 backlog. |
| 8 | Control plane delivery | Both standalone binary and Docker Compose (Compose is the primary scenario). |
| 9 | Single active control plane | Hard constraint. SQLite only on local disk; no NFS/network shares (Stage 2.1). |
| 10 | TLS | `rustls`, no system OpenSSL. Reverse proxy (Caddy/nginx) documented; optional native TLS (Stage 5.1). |
| 11 | SQLite | Bundled into the binary (`sqlx` sqlite, bundled feature), no system SQLite lib. |

## Consequences

- The vertical prototype (Stage 1) is fully runnable and testable on one
  machine with `adapter-mock`, independent of any LLM key or external service.
- Persistence, auth, and Git worktree logic are explicitly out of scope for
  Stage 1 and land in Stages 2–3. The HTTP surface and DTOs are designed to be
  stable across the in-memory → SQLite swap.

## Future

- Stage 3: pick and pin the real coding-agent CLI adapter.
- Stage 5.1: finalize TLS delivery model.
