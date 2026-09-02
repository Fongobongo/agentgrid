# Roadmap status

> Tracked summary of the competitor-feature roadmap (the detailed source
> file with origins/rationale is local-only). As of v0.4.3 everything from
> that sweep is either shipped or explicitly declined.

| Area | Status |
|---|---|
| CI-fix / merge-conflict workflow (webhooks → ci-fix template) | ✅ shipped |
| Issue-as-task (`ag issue run`, `POST /v1/webhooks/github/issues`) | ✅ shipped |
| Diff review UI + inline annotations → rework attempt | ✅ shipped |
| Command guard (deny/allow) | ✅ shipped |
| Skill/MCP security scanning | ✅ shipped |
| FTS search (`ag search`, `/v1/search`, events FTS5) | ✅ shipped |
| Shared context between attempts | ✅ shipped |
| TS/Python SDKs | ✅ shipped |
| Sandbox cold-start | ✅ benchmarked (`tests/e2e/measure-sandbox-coldstart.sh`): docker ≈ +0.5–0.9 s/spawn; image pooling not warranted |
| Multi-reviewer consensus | ✅ shipped |
| Deterministic conflict auto-resolve | ✅ shipped |
| Code knowledge graph (full) | ❌ declined — ADR 0011, trigger-based |
| Resume / tags | ✅ shipped |
| Log compression into prompts (token budget) | ✅ shipped |
| Account pool / provider failover | ✅ shipped |
| Executor–verifier role loop | ✅ shipped |
| YAML workflows in repo (`.agentgrid/workflows/`) | ✅ shipped |
| Org agents / roles / budgets / heartbeats | ✅ shipped |
| Repo learnings (`/v1/repos/{repo}/learnings`) | ✅ shipped |
| Multi-agent consensus solve | ✅ shipped |
| Context ejector (BM25 recall instead of log dumps) | ✅ shipped |
| Mobile notify + actions (web push / approve-on-go) | ✅ shipped |
| Gated role pipeline / self-healing evals / autopilot loops | ✅ shipped |
| Termux node | ✅ shipped (`docs/deploy-termux.md`) |
| Setup wizard (`ag setup`) | ✅ shipped |
| Background specialists panel | ✅ shipped |
| RSS per-attempt budgets + capacity pressure | ✅ shipped |
| CP-managed egress proxy pool with failover — ADR 0012 | ✅ shipped (v0.4.3) |

New work goes through ADRs + CHANGELOG from here; the old roadmap is
exhausted.
