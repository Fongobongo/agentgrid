# ADR 0006: Name conflict audit — "AgentGrid"

Status: accepted (Milestone 4, plan §40 naming/branding — P3); **re-audited
2026-10-31, see below**

## Context

Before a stable public release the project name "AgentGrid" must not collide
with an existing project in the same niche (distributed orchestrator for AI
coding agents). The hardening plan §40 asks for an availability audit and an
explicit decision.

## Findings (first audit, 2025-08-05)

- **GitHub repositories, same niche** (AI coding agent orchestration):
  - `naman10parikh/agentgrid` (7★) — "Spawn grids of AI coding agents (Claude, Codex, ...)"
  - `Latencius/agentgrid` (1★) — "Unified dashboard for orchestrating parallel AI coding agents"
  - `hanfeihu/agentgrid` (2★) — "Open scheduling layer for AI-operated real machines"
  - `abd-RAHEEM/AGENTGRID` (3★) — energy domain, different niche
  - `francescobianco/agentgrid` (2★) — unrelated
- **GitHub handle**: `agentgrid` user exists (0 followers); `api.github.com/repos/agentgrid` → 404 (no org with that name).
- **crates.io**: `agentgrid` package name is **free** (not published).
- **Domains**: `agentgrid.io` / `.com` / `.net` / `.org` all serve content (taken); `.dev` / `.app` respond with HTTP 402 (parked/reserved).

## Findings (re-audit, 2026-10-31)

The audit was written with a "revisit before the first stable release" trigger,
and 14 months passed. The picture is materially worse on every axis that
matters, and one finding is new in kind — a **package registry name we already
ship** is occupied.

**The niche consolidated and grew ~18×.** `search/repositories?q=agentgrid`
now returns **93** repositories (it was five). The three same-niche ones from
2025 are all still active and larger:

| repo | ★ then → now | last push |
|---|---|---|
| `naman10parikh/agentgrid` | 7 → 9 | 2026-10-21 |
| `Latencius/agentgrid` | 1 → 3 | 2026-04-01 |
| `hanfeihu/agentgrid` | 2 → 4 | 2026-06-24 |

and new same-niche entrants appeared:

- **`agent-grid/agent-grid-releases`** — "Official AgentGrid desktop releases
  for macOS, Windows, and Linux. **Run your coding agents in one visual
  workspace.**" Organization created 2026-05-11, five repos, release repo
  pushed **2026-10-30** (the day before this audit). This is a shipping
  product under the name AgentGrid, in this project's niche, released
  yesterday. It is the single most consequential finding.
- `hharsha98/agentgrid` — "multi-agent terminal grid (Claude Code, cursor-agent, Codex)"
- `Techikrish/AgentGrid` — "infrastructure for hosting AI agents on AWS"
- `lxyang20131208-star/agentgrid` — "peer-to-peer grid for renting and sharing AI agent compute"

**The top-level GitHub namespace is gone.** `agentgrid` is now an
**Organization** (created 2025-09-26, 0 public repos) — in 2025 it was a user
with 0 followers. So `github.com/agentgrid` cannot be claimed, and neither can
`github.com/agent-grid` (taken by the org above). Both obvious homes for the
canonical repository are occupied.

**Package registries — the one hard, new blocker.** The first audit checked
`crates.io` and stopped there; that was the gap.

| name | PyPI | npm | crates.io |
|---|---|---|---|
| `agentgrid` | **taken** 0.4.0 | free | free |
| `agentgrid-sdk` ← *this project's Python distribution name* | **taken** 0.1.0 | free | free |
| `agentgrid-py` | **taken** | free | free |
| `agentgrid-client` | free | free | free |
| `agentgrid-cli` | free | free | free |
| `agentgrid-sdk-py` | free | free | free |
| `@agentgrid/sdk` ← *this project's npm name* | — | free | — |

`agentgrid-sdk` on PyPI is not a squatter: version 0.1.0 (2026-04-03),
summary *"git for AI agents — registry, version control, and governance for
autonomous agent fleets"*, homepage `https://agentgrid.dev`. That is the same
niche **and** it means `agentgrid.dev` — recorded as parked in 2025 — now
belongs to a live project in this space. `sdks/python/pyproject.toml` declares
`name = "agentgrid-sdk"`, so **a PyPI publish of the SDK as it stands today
would be rejected**. Nothing publishes it yet, so nothing is broken; the
collision is latent, not live.

`@agentgrid` as an npm scope has zero published packages, but "no packages"
does not prove "unclaimed" — an org can exist with an empty registry — so it
is unknown, not free.

**Domains: not verifiable from this host.** Every one of `.io/.com/.net/.org/
.dev/.app/.sh` resolved to `127.0.0.1` in this environment because `HTTP_PROXY`
intercepts the requests, so the 2025 domain findings stand unchanged and
unconfirmed. `agentgrid.dev` is known to be in use (above); the rest are
carried forward as-is rather than re-measured badly.

## Decision

Unchanged in substance — **keep the working name "AgentGrid" for now** — but
the evidence has moved enough to change what "before the first stable public
release" has to mean in practice.

- **Still keep the name through v0.x.** Binaries, env vars, config paths and
  data dirs are all stable under it; renaming is a migration, not a rename.
  The project is at v0.7.0 and has not been publicly announced, so the
  collision has cost nothing yet.
- **The rename review is no longer "consider it at 1.0" — it is a gate that
  must be *decided* before v1.0**, because a shipping product with the same
  name in the same niche now exists. Announcing v1.0 as "AgentGrid" would put
  this project in a namespace `agent-grid` already occupies with releases.
- **The Python distribution name is a concrete blocker, not a branding
  question.** `agentgrid-sdk` cannot be published to PyPI. Either the SDK is
  renamed (`agentgrid-client` is free on PyPI, npm and crates.io; so are
  `agentgrid-cli` and `agentgrid-sdk-py`) or the SDK is not published to PyPI
  — and that is a decision someone has to make, not something to discover at
  release time.
- **crates.io `agentgrid` is still free and should be claimed early.** It is
  the cheapest defensive move available: a publish reserves the name, and the
  first audit already established it is clear.

## Trigger for a rename review

A stable public release (v1.0 announcement, public marketing, crates.io
publish) is the trigger. Migration plan (binaries/env/config/data dirs) is only
written if the rename is accepted; nothing to migrate while the decision is
pending.

Amended 2026-10-31: the trigger stays where it is, but it is now a **gate with
a deadline attached to the v1.0 milestone**, and it has a precondition that
does not depend on the rename decision — pick and reserve the registry names
(crates.io `agentgrid`, an npm scope, a PyPI distribution) before announcing
anything. Deciding to keep "AgentGrid" does not resolve the PyPI collision;
that needs its own answer.

## Method note (so the next audit is cheaper)

The first audit's blind spot was scope, not effort: it checked crates.io and
GitHub search but not PyPI/npm, and it did not re-check the GitHub namespace
kind (user vs organization). For the next pass, in this order:

```sh
gh api -X GET search/repositories -f q=agentgrid -f sort=stars -f per_page=20
gh api users/agentgrid   # .type is the thing to watch: User or Organization
gh api users/agent-grid; gh api orgs/agent-grid/repos
for n in agentgrid agentgrid-sdk; do
  curl -s -o /dev/null -w "%{http_code} pypi\n" https://pypi.org/pypi/$n/json
  curl -s -o /dev/null -w "%{http_code} npm\n"  https://registry.npmjs.org/$n
  curl -s -o /dev/null -w "%{http_code} crates\n" https://crates.io/api/v1/crates/$n
done
```

Two caveats learned the hard way: registry answers must be read as **404 =
free, 200 = taken, anything else = unknown** (a proxy or a rate limit is not a
free name), and domain checks are worthless on a host with `HTTP_PROXY` set —
they must be run from an unproxied host or not at all.
