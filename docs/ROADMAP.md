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

## Backlog after v0.6.0

Sourced from a review of 878 starred repositories cross-checked against the
code (a feature is only listed here if a `grep` proved it absent). Order is
the intended implementation order: security first, then capability, then
ergonomics. Status moves to `shipped` (with the release it landed in) as each
item is finished — this table is the single place the queue lives.

| # | Item | Status | Gap (verified in code) | Donor idea | Integration point | Size |
|---|---|---|---|---|---|---|
| 1 | Unprivileged native sandbox (`bwrap`) | ✅ shipped (ADR 0022, unreleased) | `sandbox=none` is the default and the only backends are `docker` (needs a runtime) and `systemd` scope (needs a user manager); a node with neither runs the agent with full host access | bubblewrap / nsjail / landlock | `crates/node-daemon/src/sandbox.rs` — new `SandboxKind` arm | S |
| 2 | Per-task egress allowlist | ❌ declined (ADR 0023) | `network_mode=restricted` only routes to an operator-global squid allowlist (`deploy/egress-proxy/`); a task cannot say "only `api.github.com`" | iron-proxy, OpenShell | `CreateTaskRequest.egress_allow` → node proxy enforcement | M |
| 3 | Encoded-payload scanner | ✅ shipped (ADR 0024, unreleased) | `crates/skills/src/scanner.rs` is regex-only over the literal text; a payload shipped as a bare base64/hex/percent blob produced "no findings" | SkillSpector | `crates/skills/src/scanner.rs` | M |
| 4 | `SKILL.md` import | ✅ shipped (`ag skills import`, unreleased) | discovery read `<root>/<name>/SKILL.md` and the CP had a trust ledger, but nothing turned a directory of `SKILL.md` files into that layout | awesome-openclaw-skills, anthropics/skills | `crates/skills/src/import.rs` + `ag skills import` | S |
| 5 | Symbol index | ✅ shipped (`crates/index` + `ag index --find`, unreleased) | no `tree-sitter` dependency; retrieval is BM25 over logs and `FTS5` over events, so "where is `foo` defined" is unanswerable | codegraph, codebase-memory-mcp | new `crates/index` | L |
| 6 | Attempt phase timings (6a) / OTLP export (6b) | 6a ✅ shipped (phase events, unreleased) · 6b deferred (ADR 0025) | only `agentgrid_task_duration_seconds` existed (whole task), so a slow attempt could not be decomposed; `otel.rs` is metrics-only and the `opentelemetry` feature is off by default, with no `Registry` in the tracing init for a layer to attach to | openllmetry | `crates/node-daemon/src/attempt_runner.rs` (6a), `crates/control-plane/src/otel.rs` (6b) | M |
| 7 | Eval harness (pass rate / tokens / cost) | ✅ shipped (`ag eval` + `tests/e2e/run-eval.sh`, unreleased) | `evals.rs` is a *self-healing validation* loop (pinned commands, tamper-checked), not a measurement harness: no fixture repo, no score, no regression baseline | workshop, nitpicker, deepteam | `crates/cli/src/eval.rs` + `tests/e2e` | M |
| 8 | Diff review inside approvals | ✅ shipped (`PatchView` + row toggle, unreleased) | `Approvals.tsx` showed the pending tool call only; the diff lived in `TaskDetails.tsx`, so approving a push meant leaving the queue | plannotator | `web/src/components/Approvals.tsx` | S |
| 9 | Sessions in git | queued | attempt transcripts live in the CP DB only; nothing writes them next to the code they explain | entireio/cli (git notes) | `crates/node-daemon/src/github.rs` | S |

Deliberately **not** taking (evaluated, rejected): durable-execution engines
(a workflow already survives a node loss and the CP is the durable store),
agent-memory platforms (shared context + learnings already cover it),
fleet-management UIs (the dashboard is the fleet view), and any second
sandbox *abstraction* — item 1 adds a backend, not a layer.
