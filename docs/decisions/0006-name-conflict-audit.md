# ADR 0006: Name conflict audit — "AgentGrid"

Status: accepted (Milestone 4, plan §40 naming/branding — P3)

## Context

Before a stable public release the project name "AgentGrid" must not collide
with an existing project in the same niche (distributed orchestrator for AI
coding agents). The hardening plan §40 asks for an availability audit and an
explicit decision.

## Findings (checked 2025-08-05)

- **GitHub repositories, same niche** (AI coding agent orchestration):
  - `naman10parikh/agentgrid` (7★) — "Spawn grids of AI coding agents (Claude, Codex, ...)"
  - `Latencius/agentgrid` (1★) — "Unified dashboard for orchestrating parallel AI coding agents"
  - `hanfeihu/agentgrid` (2★) — "Open scheduling layer for AI-operated real machines"
  - `abd-RAHEEM/AGENTGRID` (3★) — energy domain, different niche
  - `francescobianco/agentgrid` (2★) — unrelated
- **GitHub handle**: `agentgrid` user exists (0 followers); `api.github.com/repos/agentgrid` → 404 (no org with that name).
- **crates.io**: `agentgrid` package name is **free** (not published).
- **Domains**: `agentgrid.io` / `.com` / `.net` / `.org` all serve content (taken); `.dev` / `.app` respond with HTTP 402 (parked/reserved).

## Decision

Keep the working name "AgentGrid" for the MVP and all internal work (binaries,
env vars, config paths, data dirs are all stable under this name; renaming now
would churn every artifact and migration for no functional gain). The niche
collision is real, so **revisit the name before the first stable public
release**: if the project is publicly announced as a distributed orchestrator
for AI coding agents, pick a distinguishable name.

## Trigger for a rename review

A stable public release (v1.0 announcement, public marketing, crates.io publish)
is the trigger. Migration plan (binaries/env/config/data dirs) is only written
if the rename is accepted; nothing to migrate while the decision is pending.
