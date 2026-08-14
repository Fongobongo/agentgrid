# Changelog

## Unreleased

- **`ag index --out` + repo digest injector (Plan 1.13 follow-up).** The CLI
  `ag index` gains `--out <path>` to write the JSON packet to a file (cache /
  disk writer for the node-daemon). In node-daemon, when an operator sets
  `AGENTGRID_REPO_INDEX=1` on a node, `run_attempt` spawns `ag index --out
  <worktree>/.idx.json` during attempt prep and splices a top-levels digest
  (file → fn/type names, capped at 20 files) above the agent profile text
  written as `AGENTS.md`. Default off — every attempt spins the indexer, so
  it pays for itself only with adapters without built-in codebase awareness.
  On any failure (`ag` missing, non-zero exit, parse error) the original
  profile text is written unmodified — the slow path never blocks the agent.
  Tests: `digest_idempotent_when_env_off`, `digest_inlines_top_level_symbols`.

- **Load harness gain WS-transport variant; plan 0.3 p99 assign target achieved**
  (stage 3.1). `crates/control-plane/tests/load.rs` refactored into a shared
  `spinup_load` + a `ws_loop` mock node that connects to `/v1/node/ws`, sends
  `Hello`, and fulfils `Assignment` pushes over the WS control channel (HTTP
  data plane ack+complete unchanged). Poll-transport variant retained. New
  knobs: `AG_LOAD_TRANSPORT=poll|ws`, `AG_LOAD_WS_PROVE_LATENCY=1` gates the
  assert p99 assign < 200 ms so the target asserts only on low-contention runs.
  **Target proof**: 1 node / 2 tasks WS — `p99=47 ms`, < 200 ms.
  Scale runs (10/100) confirm `write_lock_failures=0` on both transports;
  p99 at 10/100 on a busy 3-core dev box is host-scheduler-bound (~2 s),
  not WS-arch-bound — see `docs/load-baseline-3.1.md`.

- **RSS idle baseline (plan 0.3 stage 3.2, partial).** New
  `docs/rss-budget-baseline.md` + reproducible scanner
  `deploy/dev-bench/measure-rss-baseline.sh` measuring VmRSS on the debug
  binaries: control plane idle ~52 MB (≤ 64 MB budget), node-daemon idle ~17 MB
  (≤ 25 MB). Cellar/debug scale — release slims further. Full load test
  (100 mock nodes × 1000 tasks, Stage 3.1) deferred: needs 100-node harness on
  compose or a two-host runner; CP ≤ 96 MB under WS stays a target, not a
  measurement, until that runs.

- **README: link the transport runbook.** Quickstart now points at
  `docs/runbook-transport.md` and names `AGENTGRID_TRANSPORT=poll|ws|auto`.
  Closes plan 0.3 stage 3.3 doc criterion.

- **Patch-review approval integration test (Plan 1.1 follow-up).**
  `succeeded_attempt_creates_patch_review_approval` walks the flow end-to-end
  (enroll → stdout event → complete with commit_sha → GET
  `/v1/tasks/{id}/review-approval`) and asserts a pending `ApprovalView` with
  `kind=patch_review`, the correct task/attempt ids, and scope
  `task_patch_review`. Closes the last pre-existing build-but-no-runtime test
  gap.

- **`ag index` knowledge-graph (Plan 1.13, minimal landing).** Offline
  ctags-like extraction of top-level symbols and import paths per file in a
  repo, emitted as a commit-stamped JSON packet for agents without built-in
  codebase awareness. Zero new dependencies (line-prefix dialect parser for
  rust/ts/tsx/js/jsx/py/go/c/cpp/java), deliberately top-level-only to keep
  token cost low when fed into a system prompt. Build/pruned dirs (`target/`,
  `node_modules/`, `.idx/`, `.git/`, VCS) are skipped. Two follow-ups kept
  DEFERRED: `.idx` cache layer on disk, and a `ag start` system-prompt-digest
  injector that consumes the packet (both need workspace/adapter plumbing).

- **Verifier read-only write→EPERM test (Plan 2.4 follow-up).** Closes the
  coverage gap between the docker-args substring assertion and real kernel
  semantics: `docker_ro_mount_really_blocks_write` (sandbox.rs) spawns a
  real `docker run --read-only -v <tmp>:/ag:ro` and asserts the write is
  blocked. Opt-in via `AG_RUST_TEST_DOCKER=1` (no-op on docker-less CI runners);
  the `e2e` workflow job runs it after building `ag-node:test`.

- **Capacity-pressure gate now real (Plan 2.14).** Two paired gaps
  closed: the node daemon now reports its `active_rss_mib` (VmRSS from
  `/proc/self/status`) in each heartbeat and the store persists it onto the
  `nodes` row; the scheduler SELECT now reads `active_rss_mib` and
  `max_rss_mib` (previously neither column was in the row, so `try_get`
  fell back to `(0, 1024)` and the gate never rejected on real memory
  pressure — a node could OOM and the scheduler kept dispatching).

- **Bundle-pinned skills (item 10).** A profile can declare a set of
  agentgrid skill names (`pinned_skills`, migration 0071); on apply the
  node reconciles them against the trust ledger and reports any untrusted
  pins in the apply audit (`pinned_untrusted`), fail-loud without blocking
  the config write. CLI: `ag opencode profile set --pin <name>` (repeatable);
  web: pinned-skills field + per-card line.

- **Node-readable skill-trust mirror.** `GET /v1/node/skills-trust` serves
  the operator trust ledger behind node auth. The node daemon's skill
  composition and pin reconcile use it — previously the bare `/v1/skills`
  GET from a node 401'd (it is user-JWT-only), silently emptying the
  "approved skills" prompt block and the pin reconcile.

- **Multi-node opencode-profile smoke (`tests/e2e/run-opencode-smoke.sh`).**
  A self-contained loopback smoke: temp control plane + two temp node
  daemons, one profile assigned to both, assert two apply-audit rows.
  Per-node `HOME` isolates each `opencode.json` (without it the second node
  sees the file already at the right hash and skips the audit). Verified
  locally and on the remote test host (191.96.11.161).

- **A/B percent assign.** `POST /v1/opencode-profiles/{name}/assign-percent`
  (body `{ other, percent }`) redistributes the nodes currently on either
  arm so that `percent`% land on `{name}` and the rest on `other`.
  Deterministic (ordered by node id, so re-runs are stable), only the two
  arms move, each moved node gets a ConfigUpdate push with its arm's hash.
  CLI: `ag opencode profile ab <name> --other <other> --percent N`.

- **Per-profile apply metrics.** The list route attaches an `apply_count`
  from the opencode audit feed (`SELECT COUNT(*) FROM
  opencode_config_audit WHERE profile_id = …`); the web shows it per card
  and `ag opencode profile list` prints it.

- **Import/export in the profiles UI.** Each card gets an `export` button
  (downloads its config as `<name>.opencode.json`); the upsert form gains
  an `import…` file picker that loads a JSON file straight into the
  editor.

- **Profile TTL / auto-expire.** A profile can carry an absolute
  `expires_at` (RFC3339, validated loudly on upsert). A janitor on the same
  15 s maintenance cadence deletes expired profiles exactly like a manual
  DELETE — nodes are re-pointed off via `ON DELETE SET NULL` and woken by a
  ConfigUpdate clear push, last-applied on-disk config stays. CLI:
  `ag opencode profile set <name> --config f.json --expires-at …`; web:
  "expires at" field in the upsert form + muted line on each card.

- **Delete-with-fallback for profiles.** `DELETE
  /v1/opencode-profiles/{name}?fallback=<other>` re-points every node
  currently assigned to the profile onto `<other>` in the same transaction
  that removes it (self-fallback is a 400, missing fallback a 404); the
  config push then carries the fallback hash so nodes apply it right away.
  CLI: `ag opencode profile delete <name> --fallback <other>`; web: the
  delete button asks for an optional fallback name. Plain delete keeps
  today's "nodes keep last-applied config" behavior.

- **Format button in the profile editor.** One click pretty-prints the
  JSON body (`JSON.stringify(..., null, 2)`) — no editor dependency, the
  bundle stays lean.

- **Prev↔cur diff in the profiles UI.** Each profile card's "previous
  revision" panel now renders a line diff against the current config
  (dependency-free LCS, pretty-printed JSON) with removed lines in red,
  added lines in green, plus a changed-line count in the summary.

- **Auto-heal on opencode drift.** When the heartbeat-reported
  `applied_opencode_hash` mismatches the assigned profile's hash, the CP now
  (a) writes the `opencode.drift` audit row, and (b) immediately pushes a
  `ConfigUpdate` over the ws channel so the next tick rewrites the on-disk
  file. Combined with the idempotent apply this converges within one
  heartbeat and makes a UI "drift" badge nearly always transient — which
  is why the badge itself stays unimplemented.

- **Optimistic concurrency on profile PUT.** `If-Match` (or
  `X-Expected-Hash` for clients that don't speak RFC 9110) makes two
  operators racing the same profile get a 409 with the current hash back;
  without the header the PUT remains last-write-wins.

- **`opencode debug config` oracle.** After every atomic apply the node
  shells out to `opencode debug config` and forwards the outcome
  (`verified | skipped_no_binary | verify_failed`) on the audit POST. The
  CP's `opencode_config_audit` rows now carry a `verify` column so the
  operator sees for each apply whether the binary could grep the file
  structurally rather than only "did the pull succeed".

- **Operator attribution on profile mutations.** Every `upsert` / `delete`
  / `rollback` of a profile now writes an `opencode.{action}` row in the
  generic audit feed keyed by the JWT `username` (so the dashboard's
  "who touched the stealvie" question has a name attached).

  profile row keeps the previous body next to the current one. `POST
  /v1/opencode-profiles/{name}/rollback` swaps cur↔prev, drops the far-older
  snapshot and pushes the new hash to every assigned node over the existing
  WS control channel. Web surfaces a rollback button per profile card; CLI
  gains `ag opencode profile rollback <name>`.

- **Feature "opencode profiles"**: control-plane-hosted opencode
  configuration.
  Named profiles live on the CP (`opencode_profiles` migration 0066); 6 REST
  routes CRUD them (`GET/PUT/DELETE /v1/opencode-profiles[/{name}]`), assign one
  to a node (`POST /v1/nodes/{id}/opencode-profile`, flags unknown values 404),
  audit apply events (`GET /v1/nodes/{id}/opencode-audit`), and let a node pull
  its active config (`GET /v1/node/opencode-config/active`). Per-attempt model
  overrides ride `CreateTaskRequest.opencode_override` into
  `Assignment::opencode_override` and are materialised as a
  `OPENCODE_CONFIG_CONTENT` env var for `adapter = "opencode"` spawns, merged
  over whatever profile the node last applied. Node side: WS push
  (`NodeWsMsg::ConfigUpdate { profile_id, hash }` over the existing control
  channel) triggers an atomic apply + backup; an error-threshold counter
  (config-class stderr substrings, `AGENTGRID_CONFIG_PULL_AFTER_ERRORS=3`) does
  the same as self-heal when three config errors land back-to-back. UI
  `/#/opencode` page for view/upsert/assign/delete; `ag opencode` CLI for the
  same plus per-attempt `--opencode-model/--opencode-small-model` flags on
  `ag run`. Config payload is normalised server-side through a strict
  top-level allowlist; hash-string idempotent (idempotent apply on repeat
  upsert); revision blobs stay out for now (YAGNI).

All notable changes to this project are documented in this file.

## [Unreleased]

### Added (plan 2.14)

- **Capacity pressure gate (scheduler):** nodes gained `rss_mib /
  cpu_load_pct / active_rss_mib / max_rss_mib` columns; before assigning
  a batch `try_assign` projects `active_rss + free_slots * 256 MiB` and
  rejects when it exceeds `max_rss_mib`. Each refusal lands in the
  append-only `metrics_capacity_pressure` table (timestamped, includes
  threshold + projected) so dashboards can chart "how often did we drop
  work because the node couldn't take it".

### Added (plan 2.13)

- **Background specialists panel (#26):** new `/background` route renders
  a card grid of specialist tags (`security-review`, `eval-case`,
  `consensus`, `autopilot`), each card showing count + up to three active
  attempts. Filters by capability, active/terminal status, and repo
  substring. Shares the Approvals mobile stylesheet: 44 px tap targets,
  stacked cards below 480 px.

### Added (plan 2.12)

- **Termux edge node (#24):** `deploy/install-node-termux.sh` +
  `docs/deploy-termux.md`. Prefix-based install (no systemd, no root),
  hard low-power defaults (256 MiB max_rss, 1 parallel attempt), and
  `termux-services` instructions for auto-restart.

### Added (plan 2.11)

- **Approve-on-go UI (#23a):** web approvals view reflows into stacked
  cards below 480 px (WCAG-friendly 44x44 px tap targets, focus rings,
  `data-h` cell labels, aria-label on every action). The operator TUI
  gains `a`/`d` hotkeys that arm an Approve/Deny decision against the
  newest pending approval for the focused task; the next poll tick drains
  the buffer HTTP-side so the sync `handle_key` never blocks. Swipe
  gestures and an opentui rewrite stay deferred until a real mobile
  workflow demands them.

### Added (plan 2.10)

- **Context ejector (#21):** contentless FTS5 index over task_events
  (migration 0063). On `retry_task` the CP runs BM25 against the original
  prompt, picks top-3 relevant event fragments (1 KiB cap each), writes
  them as a `resume-context-<task>.md` artifact on the previous attempt,
  and records `tokens_avoided_bytes` = full event-bytes minus digest size.
  The retry assignment prompt cites the digest name; the node fetches the
  bytes via the existing artifacts endpoint when it starts the new attempt.

### Added (plan 2.9)

- **Consensus run (#20):** `ag run --consensus 3 --models claude,codex,opencode`
  submits the same prompt to N adapters as one batch (`consensus_group_id`
  links them; each task carries its `consensus_member` = adapter name).
  When every member reaches a terminal state the CP compares the
  `changes.patch` SHA256s: agree → done; disagree → a `human-review`
  approval (scope `consensus_disagreement`) sits on one of the tasks so
  the operator picks the winning patch.

### Added (plan 2.8)

- **Repo learnings (#19):** short factual statements per repository
  persisted to `repo_learnings`. New rows start `approved = 0`. The
  scheduler injects the top-5 approved rows into every attempt prompt for
  the repo. Operators manage them via `ag learn list/add/approve/remove`
  (the CLI hits `POST/GET /v1/repos/{repo}/learnings`,
  `POST /v1/learnings/{id}/approve`, `DELETE /v1/learnings/{id}`).

### Added (plan 2.7)

- **`ag setup [--accept-defaults]`** logs in with the given (or default
  admin) credentials, saves the session token with 0600 persms, optionally
  submits a smoke task to verify the round trip, and prints the doctor
  output so the operator walks away with a green diagnostic screen.
  `--accept-defaults` makes the wizard CI-friendly (no prompts).
- **`ag doctor`** is the rapid diagnostic: server /health/ready, the stored
  session token, and the two endpoints the CLI uses most (/v1/nodes,
  /v1/tasks). Non-zero exit on any failed check; `--json` for scripting.

### Added (plan 2.6)

- **`ag autopilot "<objective>"` (#22c):** CLI loop submits a task per
  iteration, waits for terminal status, commits the local checkout on
  success and hard-resets on failure. Bounded by `--max-iterations`,
  `--max-duration` (8h default) and the first terminal failure. After the
  loop, `<summary-root>/<objective-slug>/SUMMARY.md` lists every iteration
  plus the final known-good commit so the morning-standup has a single
  file to scan.

### Added (plan 2.5)

- **Self-healing eval suite (#22b):** when a task has a
  `validation_command` and a run passes, the CP persists an
  `eval-case-<attempt>-0.yaml` artifact. Any task-level retry ships the
  accumulated suite to the node (`Assignment.eval_cases`); the node
  materialises them into the worktree at `.agentgrid/evals/` before the
  agent starts so the agent sees the obligation list, and probes every
  case through the same sandbox after the fix validates. A failing case
  flips the attempt to `eval_failed` and feeds the case log back into the
  prompt. Verifier (read-only) assignments skip materialise+probe — they
  are the enforcement side, not the producer.

### Added (plan 2.4)

- **Read-only verifier worktree (#22a):** a workflow step declared
  `role: verifier` now lands on the node with `Assignment.read_only = true`
  and the node bind-mounts its worktree as `<workdir>:/ag:ro` when sandboxed,
  so a verifier cannot silently modify the code it is supposed to validate.
  `AGENTGRID_SANDBOX_RO_HINT=true` is exported on verifier attempts so a
  verifier-aware adapter can also revert any accidental workspace writes
  (the sandbox-level `:ro` is the enforced check). Worker/integrator tasks
  remain read-write; validation continues to run read-write.

### Added (plan 2.3)

- **Sandbox cold-start benchmark (#9):** `deploy/sandbox-benchmark.sh`
  drives a control plane + node through N task iterations per sandbox
  mode (`none`/`docker`, Podman-compatible via the same docker CLI) and
  prints per-iteration submit→running ms plus a per-mode average.
  `docs/deploy/sandbox-benchmark.md` documents the method, a hand-measured
  Docker baseline (alpine `docker run` ≈0.9–1.8 s warm pull, hardened
  `--network none --read-only --cap-drop=ALL` ≈0.46 s — under the 5 s
  pool-trigger threshold), how to isolate container-start from CP tick
  cadence, and the conditional follow-up: only build a pre-warm pool if a
  production image pushes the sandbox delta past 5 s.

### Added (plan 2.2)

- **Skill/MCP security scanning (#5):** new `agentgrid-skills::scanner`
  module — a static regex catalog (17 patterns) over exfiltration sinks
  (webhooks/paste/IP-echo), instruction-override prompt injection, hidden
  shell execution (`curl|sh`, eval, base64 pipes, reverse shells,
  persistence hooks) and hard-coded secrets. Severities: Warning/Critical.
  CLI: `ag skill scan <path>` (walks a dir for `SKILL.md`, exits 1 on
  critical findings) and `ag mcp scan <id>`. Registration-time enforcement:
  `POST /v1/mcp-servers` scans the command line and rejects (422) a
  Critical finding, so a compromised third-party MCP server cannot be
  registered silently.

### Added (plan 2.1)

- **Org agents / budgets / heartbeats (#18):** long-lived agent identities
  with a role, prompt template, attached skills, a display USD budget and a
  hard task stop (`max_tasks`). Attribution flows through
  `CreateTaskRequest.agent_id` → `tasks.agent_id` (migration 0059). REST:
  `POST/GET /v1/agents`, `GET /v1/agents/{id}/actions` (immutable trail),
  `POST /v1/agents/{id}/tasks` (409 on exhausted budget). CLI:
  `ag agent add/list/actions`. Heartbeat ticker spawns the agent's prompt
  task on `heartbeat_interval_secs` (env `AGENTGRID_AGENT_TICK_SECS`,
  default 5s); every lifecycle event lands in `agent_actions`.

### Added (plan 1.12)

- **Shared context / memory (#7):** flat per-group notes for parallel
  attempts. `shared_context(task_group_id, key, value, updated_at)` table
  (migration 0058) with a (group, key) PK and upsert. Tasks opt in with a
  `group_id` (`ag run --group <id>`, `CreateTaskRequest.group_id`); the node
  forwards it to the agent as `AG_GROUP_ID`. REST surface:
  `GET/PUT/DELETE /v1/task-groups/{id}/context/{key}` and
  `GET /v1/task-groups/{id}/context` (list); CLI `ag ctx set/get/ls/del`.
  One attempt writes a note, a sibling attempt of the same group reads it;
  other groups are isolated.

### Added (plan 1.11)

- **Programmatic SDK (#8):** thin, dependency-free TypeScript
  (`sdks/ts/index.ts`, Node >= 18 `fetch`) and Python (`sdks/python/`
  `/agentgrid.py`, Python >= 3.9 stdlib) clients over the `/v1` API with a
  minimal surface: `run | wait | status | cancel | artifacts | artifact |
  login`. Auth is a JWT (`Authorization: Bearer`), from `ag login` or
  `AGENTGRID_TOKEN`. `GET /v1/tasks/{id}/artifacts` (new) lists a task's
  latest-attempt artifacts (`ArtifactMeta` gained a `name`); the SDK
  downloads one by name. `tests/e2e/run-sdk.sh` exercises the Python SDK
  end-to-end against a local control plane + mock node (no Docker).

### Added (plan 1.10)

- **Autonomous CI-fix / merge-conflict follow-ups (#1):** two GitHub
  webhooks on top of the issue webhook. `POST /v1/webhooks/github/check_run`
  spawns a CI-fix task when a check completes with `failure`/`cancelled` (the
  prompt points the agent at the check log URL). `POST
  /v1/webhooks/github/pull_request` spawns a merge-conflict resolve task
  when `mergeable_state == "conflicting"` on `opened`/`synchronize`/
  `reopened`. Both reuse the shared HMAC verifier
  (`X-Hub-Signature-256` / `AGENTGRID_GITHUB_WEBHOOK_SECRET`) and are 404
  when the secret is unset — no new config.

### Added (plan 1.9)

- **YAML workflows as code (#17):** `WorkflowTemplate::read_workflow_yaml`
  parses a strict-schema YAML file (name + step DAG with id/prompt/deps/role/
  adapter) and validates the graph before use; `ag workflow validate <path>`
  runs the same check locally (no server, exit code 1 on error) and
  `ag run --workflow <file|dir>` validates then creates+starts the run
  (a directory picks the first `*.yaml`, the `.agentgrid/workflows/`
  convention). CI validates every committed `.agentgrid/workflows/*.yaml`.

### Added (plan 1.8)

- **Account pool / provider failover (#15):** node config
  `AGENTGRID_ACCOUNTS="ENV=tok1,tok2;ENV2=tok3"` defines a per-credential
  token pool. The ACP event stream sniffs provider rate-limit errors (429 /
  rate limit / too many requests / overloaded / quota); on a hit the attempt
  runner rotates to the next token and re-drives the session without touching
  the worktree. Per-account counters (attempts / rate-limited) ride in the
  heartbeat and surface at `GET /v1/nodes/{id}/accounts/usage`.

### Added (plan 1.7)

- **Token-budget compression (#14):** `agentgrid_common::compress` —
  `dedup_lines` collapses runs of identical consecutive lines (noisy
  stack-trace / tool-output repeats) into a `…×N` marker;
  `smart_truncate` applies a hard byte cap on the last newline boundary;
  `compress` chains both and reports `saved_bytes`. The rework prompt
  pipeline (plan 1.6) now runs every annotation comment through
  `compress(comment, 4096)`, so a pasted log/diff in a review comment no
  longer blows the token budget.

### Added (plan 1.6)

- **Inline plan/diff annotations (#3b):** reviewers leave comments pinned to a
  file (and optional line range) on an attempt via
  `GET/POST /v1/attempts/{id}/annotations` (SQLite `patch_annotations` table,
  migration 0057). "Send for rework" (`POST /v1/attempts/{id}/rework`)
  creates a new task whose prompt is the original prompt plus an
  `[ANNOTATIONS]` block, so the agent takes the review feedback in a retry.

### Added (plan 1.5)

- **Executor-verifier trust loop (#16):** a rejected verifier step (the
  verifier task exits non-zero) no longer fails the run — it re-runs the
  upstream worker task with the verifier's feedback appended to the prompt,
  then resets the verifier to re-activate on the next worker success. The
  loop budget is the upstream worker's `max_attempts`; once exhausted the
  verifier rejection becomes a hard step failure. The worker step's
  `attempts` counter (1 + verifier-reject retries) is the loop count,
  exposed via `/v1/workflow-runs/{id}` `steps[].attempts`.

### Added (plan 1.4)

- **Issue-as-task (#2b CLI):** `ag issue run <N> [repo]` creates a task from a
  GitHub issue via the `gh` CLI; `ag issue ls` / `ag issue show` for
  inspection.
- **GitHub issue webhook (#2a):** `POST /v1/webhooks/github/issues` with
  `X-Hub-Signature-256` HMAC-SHA256 verification; issues carrying the `agent`
  label auto-create tasks. Secret via `AGENTGRID_GITHUB_WEBHOOK_SECRET`
  (disabled when unset).

### Added (plan 1.3)

- **FTS5 task search:** `GET /v1/search?q=` (bm25-ranked, limit 50) over a
  triggered FTS5 mirror; `ag search` CLI; search box in the web dashboard.
- **Attempt detail + resume:** `GET /v1/attempts/{id}` returns the attempt
  with the owning task's prompt; `ag resume <attempt_id>` creates a fresh
  task inheriting that prompt/context.
- **Task tags:** join table + `GET/POST/DELETE /v1/tasks/{id}/tags[/{tag}]`;
  `ag tag add/remove/list`.

### Added (plan 1.2)

- **Mobile notify webhook:** control-plane POSTs `AGENTGRID_NOTIFY_WEBHOOK` on
  terminal task states. Success with a pending patch review reports
  `awaiting_review` (not `completed`) so the operator gets a push to review.
- **Deterministic pre-merge resolve:** `deploy/pre-merge-resolve.sh` resolves
  trivial merge conflicts (both-add, import-both, formatting-only,
  delete/modify) before the LLM path. Opt-in via `AGENTGRID_PRE_MERGE_RESOLVE`;
  integrator cherry-pick/apply runs it when a conflict appears.

## [v0.3.1](https://github.com/earendil-works/agentgrid/tree/v0.3.1) - 2026-08-07

### Added (0.3 pass — final release artifacts)

- **Musl static-link release binaries:** Dockerfile.control-plane-musl produces
  `x86_64-unknown-linux-musl` control-plane + CLI, fully self-contained with web UI.
  Image `ag-cp:musl` = 11MB, no libc dependencies, runs anywhere on Linux x86_64.

- **Transport selection runbook:** docs/runbook-transport.md explains poll vs WS
  tradeoffs, deployment guidance, monitoring metrics, testing commands, and load
  baseline numbers (100-node harness: wall=30.4s, p50=21.3s, write_lock_failures=0).

### Operator Documentation

- **OPS-STARTER.md:** Quick deployment guide for Docker Compose and systemd installations,
  monitoring setup, backup procedures, and common operations.
- **TROUBLESHOOTING.md:** Comprehensive troubleshooting guide for transport issues, resource
  constraints, task execution failures, disk problems, WebSocket-specific issues, performance
  optimization, security concerns, and disaster recovery procedures.

### Metrics & Baselines

- **CP idle RSS:** 4 MiB VMRSS (well under 96 MB budget)
- **Load baseline (100 nodes):** 30.4s wall time for 1000 tasks, p50=21.3s assign latency,
  p99=29.5s, write_lock_failures=0 across both `poll` and `ws` transports
- **E2E verification:** All four transport combinations pass (`run.sh` + `run-two-host.sh`,
  both `AGENTGRID_TRANSPORT=ws` and `poll`)

### Changed (deployment fixes)

- Fixed AGENTGRID_JWT_SECRET export chain in `deploy/compose/up.sh` so compose
  interpolation succeeds reliably
- BuildKit cache-mount fix in Dockerfiles: copy to /out inside same RUN to hide binaries
  from subsequent COPY --from layers

See also:
- docs/plans/0.3-websocket-and-scale.md — full plan with stage-by-stage breakdown
- docs/load-baseline-0.3.md — detailed performance analysis and reproduction steps
- docs/node-ws-protocol.md — WebSocket protocol specification


## [v0.3.0](https://github.com/earendil-works/agentgrid/tree/v0.3.0)

*Legacy tag — superseded by v0.3.1 which includes all features plus final documentation and musl build.*

WebSocket channel implementation completed per plan 0.3 stage 2:

- ADR 0009 + node WS protocol spec
- CP endpoint `/v1/node/ws` with Bearer auth, hello/hello_ok handshake
- Node-daemon WS client (tokio-tungstenite/rustls, no OpenSSL)
- WS resilience: reconnect backoff, fencing tokens on ack path, cancel propagation
- Transport selection: `AGENTGRID_TRANSPORT=ws|poll|auto` with fallback
- E2E tests for both transports green
- Failure injection test: CP kill mid-attempt survives with durable outbox
- Initial Stage 3 load baseline measurements (Stage 3.1–3.2 finalized in v0.3.1)
