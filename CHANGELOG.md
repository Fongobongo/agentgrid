# Changelog

## [Unreleased]

### Added

- **Web UI test suite**: vitest + 7 tests over the unified-diff parser
  (`parsePatchLines`, multi-file hunks / renames / no-newline markers),
  wired into the CI web job.
- **RSS budget E2E gate** (`tests/e2e/run-rss-budget.sh`): asserts the
  AGENTS.md budgets (node idle ≤25 MiB, node streaming ≤60 MiB, CP
  ≤64 MiB) against live processes; replaces the manual rm-rss-test.sh.
- **Store race test**: 8 concurrent nodes racing one pending task must
  yield exactly one assignment (double-assign regression guard).
- **Flush throughput probe** (`tests/e2e/measure-flush.sh`): one-off
  chatty-adapter benchmark; debug build measured ~363 events/s
  end-to-end (20 000 events).
- **scripts/cleanup.sh**: codifies the dev-box disk hygiene (target
  intermediates, aged /var/tmp leftovers, optional --full/--docker/--npm).

### Changed

- **Node poll loop backs off exponentially** on failures (3s → 24s cap,
  resets on success) instead of hammering a down control plane every
  2–3 seconds.
- **Event outbox group-fsync** (`AGENTGRID_OUTBOX_FSYNC_MS`, default
  100 ms; 0 restores fsync-per-event): chatty-adapter throughput
  363 → ~870 events/s (20 000-event mock task). Machine-crash loss
  window is now ≤ the interval; process-crash durability is unchanged
  and acked events always re-anchor an fsync.

### Fixed

- **Drain E2E was silently broken**: it POSTed the drain flag as a JSON
  body while the API reads `?drain=` query params — the "undrain" step
  actually drained. The script now passes the query param, and undrain
  additionally `notify_waiters()` so parked schedulers wake immediately.

### Docs

- ADR 0001 node-channel row marked superseded by ADR 0009 (WebSocket is
  the primary channel with long-poll fallback).
- README gains a **single-binary quickstart** (download release tarball →
  run → setup in the embedded UI) and the maturity table now reflects the
  UI being embedded in the binary.

### Maintenance

- Dead RUSTSEC-2023-0071 suppression removed from deny.toml and
  supply-chain.yml (sqlx 0.9 no longer pulls `rsa` transitively).

## [v0.4.1] — 2026-08-30

### Added

- **Release workflow: boot-the-binary sanity gate in publish** — the
  packaged x86_64-musl tarball is started on the runner and must answer
  `/health/ready` and serve the embedded UI before signing/publishing.
- **Approvals UI: bulk-approve all pending** with a confirmation modal.
- **New Task: live request preview** (the exact JSON POSTed, incl. via
  consensus fan-out and GitHub write-back).

### Fixed

- **Musl scratch image no longer boots to a missing data dir.** Dropping
  the `web/dist` copy (now embedded) also removed the only step that
  created `/var/lib/agentgrid`; the container crashed opening the SQLite
  db. The dir is materialized from the builder stage.
- **Release tarballs carry the exec bit again.** download-artifact v4
  strips modes — the v0.4.0 assets shipped 644 binaries. (On v0.4.0 run
  `chmod +x` after untarring.)

### Maintenance

- `docs/plans/0.3-websocket-and-scale.md` removed (shipped by v0.4.0).
- `.dockerignore` added (target/, node_modules, .git, dbs, plan docs) —
  smaller build contexts.
- Release workflow: cosign sign-blob now retries up to 3× on transient
  Sigstore timestamp-service failures.
- Release workflow: `chmod +x` before packaging — download-artifact v4
  strips the exec bit, so the v0.4.0 tarballs shipped non-executable
  binaries (fixed above).

## [v0.4.0] — 2026-08-29

### Added

- **Web UI embedded into the control-plane binary (rust-embed).**
  `agentgrid-control-plane` now serves the full React UI with no side
  files: release binaries, containers and bare checkouts all show the UI
  out of the box. Priority is unchanged — `AGENTGRID_WEB_ROOT` overrides,
  then an exe-relative `web/dist`, then the embedded assets. SPA fallback,
  security headers (`CSP`/`nosniff`/`DENY`), `no-cache` for index.html and
  immutable caching for hashed assets are shared between the filesystem
  and embedded paths; a build.rs placeholder keeps bare `cargo build`/
  `cargo test` working without the npm step.
- **Web UI parity push — the operator surface no longer CLI-only.** New
  panels: Users (list + create), Agents (register + action trail), Agent
  Profiles (revisions + activate), Learnings (add/approve/revoke/delete per
  repo), Conversations (list + multi-turn chat), Shared Context (per-group
  key/value notes), MCP servers (registry + delete), Repositories (add),
  Admin (SQLite backup + storage GC dry-run/run + autonomy policy dry-run),
  Workflow authoring (template create from steps + fire run + interval
  schedules).
- **Nodes panel:** one-shot enrollment-token minting (copyable) and
  per-node credential-pool usage (env / token idx / attempts / rate-limits).
- **Opencode profiles:** A/B rollout via `assign-percent` directly on the
  profile card.
- **New Task form: full `CreateTaskRequest` surface** — advanced section
  with base_commit, network_mode, security_profile, max_attempts,
  ACP session resume, task group, agent attribution, opencode model
  overrides, GitHub write-back (repo/issue/base-ref) and multi-adapter
  consensus runs (N tasks, one group id — same semantics as
  `ag run --consensus N --models …`).
- **Task details: editable tags** (add/remove chips inline).
- **API: `GET /v1/conversations`** — list conversations newest-first (the
  route previously only supported create/show-per-id; store gains
  `list_conversations`).

### Removed

- **Meta-plan docs trimmed from the repo.** Completed or superseded planning
  files removed to cut documentation noise for new readers (git history
  retains them): `agentgrid-hardening-plan.md` (641/642 items done; the one
  deferred item moved to issue #52), `docs/plans/0.2-completion.md` (plan
  fully delivered), `docs/plans/0.4-production-ready.md` (was already
  deprecated — k8s/Postgres path rejected against the SQLite-only
  constraints), `docs/plans/windows-deploy-*.md` (Windows is not a supported
  target; unreferenced). The gitignored competitor roadmaps stay.

### Security

- **RUSTSEC-2025-0134 closed: rustls-pemfile dropped from the workspace.**
  The TLS loader now uses `rustls-pki-types`' `PemObject` (pki-types ≥ 1.11,
  already in the rustls 0.23 graph) for cert/key PEM parsing; the archived
  crate is gone from the dependency tree and the advisory ignore removed
  from `deny.toml`. For a rustls-only project this was the most
  supply-chain-sensitive RUSTSEC warning on the board.

### Fixed

- **GitHub Release body was empty for tagged releases.** The release
  workflow extracted the `## [vX.Y.Z]` section with an exact `$0 ==` awk
  comparison, but real headers carry a suffix ("## [v0.3.9] — 2026-08-29"),
  so every tagged release published an empty body ("No changelog entries").
  Now a prefix match, verified against the v0.3.9 section.

### Added

- **Image smoke test in CI (`tests/e2e/run-image-smoke.sh`).** Boots the
  built images and verifies liveness end to end: the musl control-plane
  (`FROM scratch`, no shell — deliberately no HEALTHCHECK) must serve
  `/health/ready` and a setup+login+task-list round trip on a real volume;
  the glibc control-plane and node-daemon must reach docker `Health=
  healthy` via their HEALTHCHECKs. Catches the v0.3.7 bug class (a
  HEALTHCHECK that could never succeed in its own image) at CI time.
- **Scope-creep guard integration test (false-positive cases).** Drives
  the scan through `TaskLifecycleService::complete_attempt` like the HTTP
  layer: one `scope_creep` audit event for an unrequested `md5sum` tool
  call; silence when the prompt mentions hashing; silence for hash-free
  attempts.
- **Property-style test for the agent-budget write-gate race.** Deterministic
  (workers × max_tasks) grid asserting concurrent attributed creations let
  exactly `min(workers, max_tasks)` through and spend exactly that many —
  the check-then-act race class from the v0.3.6 audit.

### Maintenance

- **Transitive RUSTSEC advisories patched via Cargo.lock:** event-listener
  5.4.1 → 5.4.2 (unsound `StackSlot` `!Send` crossing, RUSTSEC-2026-0221;
  via sqlx-core) and lru 0.18.1 → 0.18.3 (use-after-free in
  `LruCache::pop()` panic path, RUSTSEC-2026-0253; via ratatui/TUI). Both
  fix-release-only bumps, no code change. Issues #38/#39 closed.
- **Repo description + topics set** (`rust`, `ai-agents`, `orchestrator`,
  `llm`, `coding-agents`); previously empty.
- **README: "Why this instead of k8s Jobs + cron" section on the first
  screen** — the trust/review/zero-deps/human-gate story before any install
  steps; status line un-staled (0.3.2 → 0.3.9).

## [v0.3.9] — 2026-08-29

### Fixed

- **Load-harness WS nodes now reconnect on socket loss.** A dropped
  mid-run WebSocket left `ws_loop` spinning on an ended stream until the
  drain deadline (2 of 4 runs at 100 nodes / 1000 tasks stalled at
  946/1000 on the remote load host); the loop now re-hellos with a fresh
  free-slots heartbeat, letting the CP reassign the in-flight attempts.

### Added

- **Plan 0.3 stage 3.1/3.2 full-scale load results (remote host).** The
  deferred 50/1000 and 100/1000 cassettes ran on the idle test host
  `191.96.11.161` (2 vCPU / 4 GB): poll and WS transports, 1000/1000
  completed, `write_lock_failures=0` at 3.7–4.7k write txns per run, task
  list reads p99 ≤ 622 ms, RSS (whole in-process harness incl. mock
  clients) 86.5 MiB at 50 WS nodes — inside the 96 MiB budget. Reports:
  `docs/load-baseline-3.1.md`, `docs/rss-budget-baseline.md`; plan
  0.3 items 3.1 and 3.2 closed.

- **Scope-creep guard: audit unrequested hash/checksum busywork.**
  Competitor-gap feature (stop-that-shit-inspired) — on a successful
  completion the control plane scans the attempt's command-bearing events
  (tool/stdout/stderr/error) for hash/checksum commands the prompt never
  asked for (`md5sum`/`sha256sum`/`openssl dgst`/…) and appends audit-only
  `scope_creep` events (`unrequested_hash`, or `mass_hash` for
  `find|xargs`-style whole-tree runs). Prompt containing any hash word
  silences the scan (conservative). Findings never change the outcome,
  land in the event log and are searchable via `GET /v1/search/events`.

## [v0.3.8] — 2026-08-28

### Added

- **Project brain: `ag brain` + AGENTS-BRAIN.md prompt block.** Competitor-gap
  feature — `ag brain <repo>` rebuilds a persistent `AGENTS-BRAIN.md` from the
  repository's task history (prompt + outcome + error category per terminal
  task). Every attempt (ACP and wrapper paths) appends the file as a "Project
  brain" block to the prompt when it exists in the worktree — a persistent
  project memory that survives per-attempt worktree isolation because it lives
  in the repo like any tracked file. Absent/unreadable = no block (a hint,
  never a hard dependency); the block is capped at 8 KiB so a bloated brain
  cannot eat the prompt budget.

- **Verification note: claimed finish vs actual commit.** Competitor-gap
  feature (babysitter-style deterministic check) — on a successful completion
  the control plane cross-checks what the agent CLAIMED (a `result` event)
  against what it actually PRODUCED (a commit) and appends an audit-only
  `verification_note` event on mismatch: a commit without a result event is a
  "silent success" (diff exists, nobody claimed the finish). The note never
  changes the outcome, lands in the event log, and is searchable via
  `GET /v1/search/events`. The mirror case (result without commit) is
  deliberately NOT flagged — on a success that means a plain-dir task with no
  commit by design, and a note there was pure noise that broke stdout-sequence
  contiguity checks in e2e.

- **Egress firewall for restricted-mode sandboxes (`deploy/egress-proxy/`).**
  Competitor-gap feature — docker has no native per-domain egress filter, so
  `network_mode=restricted` collapsed to `--network none` (safe, but network
  fully off). A squid sidecar now restores "internet but allowlisted domains
  only": with `AGENTGRID_SANDBOX_EGRESS_NETWORK` +
  `AGENTGRID_SANDBOX_EGRESS_PROXY` set, restricted attempts attach to the
  proxy network and get `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` injected; the
  proxy (read-only, cap-dropped, no caching) denies everything not in
  `squid.allowlist.conf`, including RFC1918 LAN ranges. Unrestricted/none
  attempts never inherit the proxy env, and the heartbeat's
  `enforced_limits` stays honest (restricted-on-proxy still counts as
  egress-isolated).

- **Events search: `GET /v1/search/events`, `ag search --events`, dashboard
  toggle.** Competitor-gap feature — the contentless `events_fts` mirror
  (migration 0063) previously only fed the retry resume-digest; past agent
  events (logs, tool calls, results) are now publicly searchable. Hits join
  `task_events` → `attempts` for the owning task id and carry sequence +
  stored event type + raw payload, ranked by bm25 with a server page cap of
  1000. UI gets a Tasks/Events toggle on the dashboard search box.
- **Diff review: numbered, commentable diff + rework via annotations.**
  Competitor-gap feature — the `patch_annotations` backend existed but the
  web UI never used it. The changes.patch viewer now parses unified-diff
  hunks into file + line-numbered rows, lets an operator click any added/
  context line to attach an inline comment (persisted through
  `POST /v1/attempts/{id}/annotations`), lists comments above the diff, and
  "Request rework" now goes through `POST /v1/attempts/{id}/rework` (a fresh
  task with all annotations folded into the prompt) instead of a bare retry.
- **GitHub write-back: push branch + open PR + comment on issue.**
  Competitor-gap feature — the issue webhook (`agent` label) and
  `ag issue run --push` now stamp the task with `github_push`,
  `github_repo` (owner/name), `github_issue` and `github_base_ref`
  (migration 0076, echoed in `TaskView`/`Assignment`/web/CLI). After a
  successful finish the node pushes its agent branch from the bare mirror,
  creates a PR via the REST API and comments on the linked issue — before
  the worktree is reclaimed. Requires `AGENTGRID_GITHUB_TOKEN` on the node;
  strictly best-effort: any failure emits a `github_*` log event and never
  fails the task, and the token never reaches logs, git config or the
  adapter.

- **Consensus patch review: N reviewers judge one diff.**
  Competitor-gap feature (nitpicker-inspired) — `ag review <task>
  --models a,b` fires one review task per adapter over the task's
  `changes.patch` (truncated at 100k chars), grouped under
  `consensus_mode = 'review'` + `review_of` (migration 0079). On group
  collapse the CP reads each reviewer's latest `result` event: unanimous
  APPROVE auto-approves the pending patch review; any
  REJECT/unclear/absent verdict leaves it for a human. One collapse
  decision per group (audit marker under the write gate), so concurrent
  member completions cannot double-fire.

- **Task-level auto-retry: `max_attempts`.** Competitor-gap feature
  (hatchet-inspired) — `CreateTaskRequest.max_attempts` (default 1 = no
  auto-retry, migration 0078, echoed in `TaskView`; `ag task run
  --max-attempts`). When an attempt FAILS and fewer than `max_attempts`
  attempts have run, `complete_attempt` re-queues the task instead of
  leaving it failed — same semantics as manual `retry_task`, including the
  post-commit resume digest and a `retry.auto` audit entry. Cancellation
  is never auto-retried; the attempt row stays failed for the audit trail.

- **Built-in trivial conflict resolution.** Competitor-gap feature
  (GitWand-inspired) — `AGENTGRID_PRE_MERGE_RESOLVE` stays the operator
  hook, but when it is unset the node now resolves only safe hunks itself:
  identical both-sides, one side empty, whitespace-only differences.
  Anything else leaves the tree untouched and the cherry-pick fails as
  before (best-effort, never silent).

- **Convergence metrics: validation rounds + rework chain.**
  Competitor-gap feature (loop-engineering-inspired) — the node reports
  `validation_rounds` per completed attempt (migration 0077, echoed in
  `CompleteAttemptRequest`/`AttemptView`/web) and tasks carry `rework_of`
  linking a rework task to the attempt that spawned it. The dashboard shows
  both, so a task that bounced through several review→rework iterations is
  visible at a glance instead of looking like fresh work.

- **Skills scanner: agent-security checklist.** Competitor-gap extension
  — the skills scanner (prompt-injection/instruction-override) now also
  flags agent-safety patterns: secrecy instructions, silent exfiltration,
  reading credential files, dumping env to the network, git-hook
  persistence, data URIs, URL shorteners, zero-width Unicode and
  privileged `curl | bash`.

- **Diff pattern-scan on `changes.patch`.** Competitor-gap feature — on
  success the CP scans the produced diff for risky patterns (secret
  material, absolute paths, debug leftovers) and appends audit-only
  `diff_finding` events instead of blocking the result.

- **Auto-learnings from review annotations.** Competitor-gap feature
  (claude-reflect-inspired) — a rework annotation with a *lesson* marker
  lands in the per-repo learning backlog (`ag learn list`) with
  `approved = 0`; an operator approves before it surfaces in agent
  prompts, so review feedback becomes durable repo memory instead of
  disappearing with the rework task.

## [v0.3.7] — 2026-08-26

> Post-release audit of v0.3.6: 20+ confirmed logic, durability and
> performance bugs across control-plane, node-daemon, web UI and
> deploy. Full descriptions in the sections below.

### Fixed

- **Retry path panicked on non-ASCII event payloads.** The resume-digest
  fragment cap sliced the BM25 payload at byte offset 1024; a Cyrillic/CJK
  log line whose codepoint straddles that offset panicked inside
  `bake_resume_digest`, so `POST /v1/tasks/{id}/retry` returned 500 and the
  task could never be retried. The cap now lands on a char boundary.
- **A self-reported-offline heartbeat could kill FRESH attempts.** The
  heartbeat's status flip and its `lose_node_attempts` sweep ran in two
  separate transactions; a poll re-onlining the node inside the window (and
  the scheduler handing it new assignments) saw those attempts failed as
  `node_lost` by the stale sweep. The sweep now re-checks the status inside
  its own write transaction and only fires on a still-`offline` node — same
  discipline `mark_offline_nodes` already used.
- **Agent budget hard-stop was check-then-act.** `create_agent_task` read
  the spend and inserted the task in separate statements, so two concurrent
  creations could both observe `tasks_spent = max - 1` and both insert,
  exceeding `max_tasks`. The spend read, budget check, trail row and insert
  now commit in one `BEGIN IMMEDIATE`; 8 concurrent creations against a
  budget of 3 land exactly 3.
- **Artifact quota check failed open on a storage error.** The upload path
  read the used-bytes figure with `.unwrap_or(0)`, so during a DB incident
  every upload passed the quota check; it also re-read
  `AGENTGRID_ARTIFACT_QUOTA_MB` from the env per upload. The quota is now
  captured once at startup (`Limits.artifact_quota_bytes`) and a failed
  usage read maps to 503 instead of "0 bytes used".
- **Overlapping schedule ticks could double-fire a workflow run.** The tick
  ran from both the maintenance loop and startup reconcile, read
  `last_run_at`, created the run, then stamped it — three separate
  statements. The fire slot is now claimed first with a CAS on
  `last_run_at`; only the tick that moves the row creates the run.
- **The batch scheduler ran N+1 queries inside the single-writer
  transaction.** `try_assign_batch` resolved role, attempt-count, eval-case
  and repository lookups per candidate while holding the process-wide write
  gate — a full 100-slot batch issued ~500 statements and queued every other
  write behind it. The invariant lookups are now four bulk `IN (...)`
  queries before the flip loop; the txn keeps only the CAS UPDATE + INSERT.
  The duplicated per-candidate `attempt_count` (computed twice) collapsed
  into the bulk count.
- **Two maintenance pipelines had drifted.** `tick_maintenance` (startup
  reconcile) and the spawned background loop reimplemented the same
  sequence, and only the loop ran approval expiry — a restart silently
  skipped it. The loop now calls `tick_maintenance`; backups and the WAL
  checkpoint cadence stay on the loop side.
- **Small control-plane fixes.** The per-node event-rate map grew without
  bound (one entry per enrolled node; now pruned past 1024 keys like the
  login limiter). The one-time setup token was consumed before the first
  user was created — a transient DB failure burnt the bootstrap token until
  restart (now consumed only after a successful create).
  `assign_percent_between` used a deferred transaction, bypassing the
  single-writer gate other mutations hold (`SQLITE_BUSY_SNAPSHOT` exposure;
  now `write_txn`). A session-revocation check failure mapped every request
  to 401 without logging — still fail-closed, but the root cause is logged
  now.
- **The builtin command-policy classifier missed obvious danger.** A plain
  `curl` (no literal `-X POST`) fell through to ExecuteLocal and was
  auto-allowed at L2+; `rm --recursive --force` bypassed the short-flag
  destructive check; `echo` and `git` were dead entries in the read list
  (shadowed by earlier arms). Every curl is now NetworkWrite, rm's long
  flags count as destructive, and the dead read-list arms are gone.
- **An ACP spawn failure skipped completion, outbox drain and worktree
  cleanup (node-daemon).** `drive_acp_session` propagated the
  `cmd.spawn()` io error with `?`, so on EMFILE / a binary vanishing
  between resolve and exec the attempt sat `running` until the CP reaper,
  the worktree leaked to the 24 h prune, and ND-4 redelivery re-ran the
  whole task. A spawn failure now returns the same `infrastructure_failed`
  result as the missing-binary arm, flowing through the normal
  finalize/report/cleanup path.
- **Worktree + branch leaks on early-exit attempt paths (node-daemon).**
  The ack-rejected (ACP and wrapper) and adapter-missing paths returned
  after `prepare_workspace` but before `cleanup_workspace` — the worktree
  dir/gitlink survived to the 24 h prune and the `agent/<task>/<n>` branch
  was never reclaimed. All post-prepare exits now reclaim via a shared
  helper. Related: in `cleanup_workspace` itself the `branch -D` sat behind
  the fallible `worktree remove`, so any remove failure skipped the delete;
  it runs best-effort regardless now.
- **ACP completions dropped plan / base commit / finish remote head
  (node-daemon).** The ACP `report_complete` call hardcoded `None` for the
  Stage-13 plan, `resolved_base_sha` and `remote_head_at_finish`, so the
  PlanReady pause never fired for any ACP adapter (the primary protocol)
  and the start→finish drift audit was void on every ACP attempt. All
  three are captured and shipped like the wrapper path already did.
- **No HTTP/WebSocket timeouts anywhere in the node-daemon.** reqwest and
  tungstenite default to no timeout, so a connection that accepted but
  never answered parked the poll loop, heartbeat, event flusher or WS
  session setup forever — `send_with_retry`'s attempt-count bound never
  fired. All production clients now carry a 10 s connect + 120 s total
  timeout, and the WS handshake is bounded at 15 s.
- **Unbounded child reaps and a blind SIGKILL escalation
  (node-daemon).** The wrapper-supervisor and validation paths waited on a
  killed child forever — a process in uninterruptible disk sleep survives
  SIGKILL and would park the attempt, leaking its concurrency slot (the
  ACP path already bounded this); all reaps are capped at 15 s now.
  `terminate_group` also escalated to SIGKILL after 10 s without checking
  the group still belonged to our child — a recycled pgid would have been
  killed; escalation now stops early when waitpid reaps or loses the
  child.
- **Git token validation missed leading dashes; outbox crash leftovers
  were never swept (node-daemon).** A CP-supplied token starting with `-`
  would parse as an option, not a rev, in `git cherry-pick <sha>` /
  `git fetch origin <sha>` (defense-in-depth; rejected now). Orphaned
  `.tmp` stage siblings from ack/record/compact crashes and stale
  quarantine entries accumulated forever — invisible to the quota's
  `.jsonl` filter; startup recovery sweeps them past the orphan age cap.
- **The musl image's HEALTHCHECK could never succeed (deploy).** The final
  stage is `FROM scratch` — no shell, no wget — so the check always failed
  and the container reported permanently unhealthy. Removed; liveness is
  the orchestrator's job. The demo compose also started nodes with an
  empty enroll-token default that crash-looped without a hint; both tokens
  are required (`:?`) like the production compose now.
- **The web UI silently truncated every list at ~100 rows (web).** The
  `next_cursor` field was declared but never read and no caller passed a
  `limit`, so task #101 onward was invisible with no indication. `listGet`
  now auto-pages through the server's keyset cursor
  (`after_created_at`/`after_id`) until the page runs short.
- **The task-details terminal refresh trigger could never fire (web).**
  The status-event handler checked for a string payload, but every status
  event carries an object payload (`{"status":"validating",...}`) — a task
  completing while the page stayed open never refreshed, so the
  diff/review sections never appeared without a manual reload. The queued
  eligibility banner also stayed stale forever; it refetches on events
  while the task is queued.
- **Nodes audit rows could render under the wrong node (web).** Rapid
  A→B expander toggles let A's slow response resolve after B's and
  overwrite the audit table; rows now carry their owning node id and a
  stale response renders as "loading". `sseConnect.close()` now cancels
  the in-flight reader instead of waiting up to ~15 s for the next server
  keep-alive, and the expanded row's colSpan matches the 12-column table.
- **Web cleanup.** OpencodeProfiles re-declared `OpencodeProfile` /
  `NodeView` locally with drifted fields and read the list envelope by
  hand — it uses the canonical `api.ts` types and helpers now (the
  revision fields moved into the shared interface). The capability
  vocabulary in Background is module-local, not an unused export.
- **The workflow budget tick re-read the whole run history every 5 s
  per running run.** `workflow_tokens_cost` fetched every metric event
  payload into Rust to sum tokens/cost — O(run lifetime) on a fixed
  cadence. SQLite's `json_extract` aggregates server-side now, driven by
  the new `idx_events_type` index (migration 0074) over the small `metric`
  subset; the repeated-handoff scan keeps full semantics (a pre-broadcast
  streak must survive) and stays on the indexed `(run_id, sequence)` range.
- **The workflow run projection endpoint was N+1 across steps.** Every
  `GET /v1/workflow-runs/{id}/projection` (polled by the UI and ACP)
  issued ~4 queries per step: role_runs lookup, task status, latest
  attempt and latest result text. The role_runs and latest-attempt
  lookups are bulk queries now (window-function ROW_NUMBER for "latest"),
  leaving O(1) round trips plus one capped result-text read per linked
  task.
- **The node-daemon image shipped no HEALTHCHECK (deploy).** `restart:
  unless-stopped` cannot catch a hung daemon. The heartbeat loop now
  touches a `heartbeat.stamp` marker under AGENTGRID_DATA_DIR after every
  iteration, and the image's HEALTHCHECK fails when the stamp is older
  than 30 s (3× the default interval).
- **Four web views polled on timers, defeating the change-stream design.**
  Approvals (3 s), Skills (5 s), Background (2 s) and OpencodeProfiles
  (5 s) re-fetched on intervals while Dashboard/Nodes/Audit/Workflows used
  the `/v1/stream` fingerprint channel; the polling views now use
  `useLiveRefresh` too — idle pages make no requests, changes land in under
  a second. The hand-rolled `if (!r.ok) setError(...)` blocks (drifted
  messages that lost the status) collapse into a shared `reqOk` helper /
  typed `ApiError`.
- **The repeated-handoff circuit breaker is O(1) now (migration 0075).**
  Every budget tick rescanned the run's whole `workflow_messages` history
  to recompute the streak. `emit_workflow_message` maintains the live
  streak, its all-time max and the last `(from, to)` pair in the same
  transaction as the insert — the breaker reads the max column (a runaway
  stays tripped even after a healthy broadcast resets the live streak).
  Startup reconcile replays history once for runs predating the migration.

### Added

- **Index `role_runs(task_id)`** (migration 0073). The scheduler role probe,
  unacked assignments, workflow routing on attempt completion and the run
  projection all filter `role_runs` by `task_id`; only the `step_run_id`
  index existed, so each lookup scanned the full workflow history.

## [v0.3.6] — 2026-08-23

> Quality audit after the v0.3.5 stabilization: 16 confirmed logic and
> durability bugs in control-plane/node-daemon, ~350 lines of dead code,
> collapsed duplicates (sha256, fencing header, keyset pagination, SSE
> loops, supervision core), a concurrent gateway, an ACP frame cap and a
> nightly cargo-fuzz job in CI. Full descriptions by finding id below.

### Fixed

- **The claim/link wedge recovery itself could duplicate step tasks (audit
  X-C2b).** The v0.3.6 wedge reset treated *any* running step without a
  task link as wedged — but the window between a winning CAS claim and
  `set_role_run_task` is exactly that shape while `create_task` is slow
  under contention (20 concurrent ticks produced 3 spawns). The reset now
  requires the claim to be older than a 60 s grace period (measured from
  `started_at`), so healthy in-flight claims are never disturbed and true
  crash-wedges still recover.
- **The gateway bot froze for up to 300 s on every `/run` (audit X-G1).**
  Update handling ran inline in the Telegram poll loop, so awaiting a task's
  answer blocked everything — including `/cancel` from any chat — until it
  finished. `ControlPlane` is owned/`Clone` now and each update is handled
  on its own task; the poll loop never blocks.
- **The gateway's answer watcher counted events positionally (audit
  X-G2)** — `.skip(seen)` over a full re-fetch of the events list desynced
  if the response was ever windowed or trimmed, skipping or double-counting
  turns. It resumes by `after_sequence` cursor now.
- **Adapter/oracle probes could hang the heartbeat and WS session
  indefinitely (audit X-B10).** `--version` probes and the opencode config
  oracle ran unbounded `.output().await`s inside sequential loops; one wedged
  binary stopped all heartbeats (node swept offline) or stalled pings until
  the socket flapped. Both are time-bounded now.
- **Torn `.tmp` stage files were uploaded as bogus artifacts (audit X-B8)** —
  a crash between staging and publish left a partial file that startup
  recovery shipped under its temp name and `pending_artifacts` advertised
  forever. Spool listing skips them.
- **Definitively-rejected completions were retried forever (audit X-B12).**
  A 4xx fencing/transition rejection left the durable record in place to be
  re-sent with a doomed token once per restart. Mirror of the artifact
  policy: definitive statuses (400/401/404/409/412/413/422) drop the record
  in both the live send and startup redelivery paths.
- **The outbox drain deadline was soft by ~26 s per stuck chunk (audit
  X-B14)** — each chunk carried the full retry budget. Drains are best-effort;
  they use a short budget now (the durable outbox retains the rest).
- **A worktree removed via the rm-fallback kept its stale gitlink until the
  next restart prune (audit X-B15)** — a same-id retry failed `worktree add`
  in between. Prune runs right after the fallback removal now.
- **The pruner recursed into its own `.quarantine` dir (audit X-B17)**,
  producing rename-on-self warn spam every startup. Skipped.
- **The config-error classifier substring-matched noise (audit X-B18)** — a
  bare `"401"` pattern fired on byte offsets / ids / line numbers inside
  arbitrary error payloads, incrementing the self-heal streak spuriously.
  It requires the JSON-quoted form now.
- **The `completion_rows` heartbeat gauge counted ack drop-markers as
  pending rows (audit X-B19)**, over-reporting after every ack-before-
  compaction. Markers and dropped records are excluded now.
- **`remote_head_at_start` was captured after the agent finished (audit
  X-B9)**, making it identical to the finish value and voiding the drift
  audit field. It is captured at attempt start and also populated on the
  ACP path, which used to hardcode None.
- **Conversation turns never recorded the agent's answer (audit X-C6b).**
  `compose_conversation_prompt` renders `assistant:` turns from history, but
  nothing ever wrote that role — every follow-up prompt silently omitted
  what the agent had answered before. A successful completion now echoes
  the attempt's latest `result` event as an assistant message on the
  conversation that spawned the task (best-effort; a NOT EXISTS guard makes
  completion redelivery idempotent).
- **The Dashboard FTS search had a response-ordering race (audit X-W5)** —
  a slow earlier query could resolve after a newer one and overwrite the
  fresher results. Only the latest in-flight response lands now.
- **The ACP server read loop had no frame-size cap (audit X-A1).** The
  client side bounds one inbound line at 1 MiB, but the server used
  `BufReader::lines()` — a runaway or malicious peer streaming one
  unterminated line grew server memory without bound. Frames are now capped
  like the client's; an oversized frame is drained to its end and skipped,
  and the session survives.
- **A failed request send leaked its entry in the ACP client's pending map
  (audit X-A2).** The id was inserted before the write; on a fast send
  failure it stayed until transport teardown. It is now removed on the
  error path.
- **The workflow-run badge rendered unstyled for failed/running runs
  (audit X-W1).** The Workflows view kept a second, drifting `statusClass`
  whose `err`/`run` classes have no badge styles; the run summary now uses
  the shared vocabulary from `util`.
- **Opening a running task never revealed its diff/review section when the
  task finished (audit X-W3).** The task object was fetched once on mount;
  a terminal status event on the live stream now re-fetches it.
- **OpencodeProfiles hand-rolled four `fetch()` calls around the central
  API client (audit X-W4)** — losing the shared expired-session handling.
  They route through `req()` now.

### Changed

- The two SSE reconnect loops (`streamTask` / `streamChanges`) are extracted
  into one shared `sseConnect` helper — same backoff, 401→login handling,
  and reader pump; `streamChanges` now also surfaces connection errors
  through the optional callback instead of swallowing them.
- Shared keyset-pagination scaffolding (`KEYSET_PREDICATE` / `KEYSET_ORDER`
  / `page_limit`) replaces six copy-pasted copies across the workflow,
  task, repository, and approval list queries — the copies had already
  drifted (only one carried the binding-order note).
- Deduplication pass (audit): one shared `agentgrid_common::sha256_hex`
  replaces the four independent implementations and inline digests
  (control-plane store/opencode profiles/routes, node-daemon polling/
  opencode-config, skills hash) — the opencode-profile hash round-trips
  CP↔node, so all sites must agree on canonicalization. The fencing-token
  header literal is now a single shared constant (`FENCING_TOKEN_HEADER`)
  across both crates instead of eight copies. The CLI no longer silently
  drops an invalid stored session token (which produced unauthenticated
  requests with confusing 401s) — it fails fast with a clear error.
  Intentionally kept: explicit `CreateTaskRequest` literals (compile errors
  on new fields force a conscious decision per call site).

### Removed

- Dead code sweep (audit): the unused `to_event_kind` adapter mapping
  (superseded by the ACP envelope translation), `Store::user_exists`,
  `config_error::current_streak`, the test-only `CommandGuard::from_env`,
  the never-called `opencode_config::clear_applied_hash` (the keep-last-
  known-good profile decision makes it intentionally unreachable), the
  unparsed bundle-manifest types (`SkillBundle`/`SkillRef`/`LockEntry`/
  `BundleSource`/`SkillPin`) and the never-wired `RevisionStore`
  rollback feature with its tests, the test-only `has_critical` /
  `SkillCatalogEntry` helpers, the empty `ReworkRequest` placeholder, the
  unused `common::rss` module (heartbeat has its own probe), and the
  documented-but-never-sent WS close code 4004 (const + protocol-doc row).
  The stale `#[allow(dead_code)]` on `RequestId` (actually read by attempt
  handlers) is dropped too.

### Fixed

- **A secret straddling the line-cap leaked through the redactor's
  newline-truncation path (audit X-N3).** The streaming redactor carries a
  trailing overlap when it force-splits a newline-less overflow, but the
  over-long-line-with-newline branch dropped everything past the cap with no
  overlap — a key crossing `AGENTGRID_MAX_LINE_BYTES` was published as an
  unmasked fragment. Both branches now carry the same overlap; the middle of
  the line is also streamed (masked) instead of silently discarded.
- **A timed-out eval probe could hang the attempt forever (audit X-N4).**
  Eval commands run as their own process group but the timeout arm killed
  only the direct child — any grandchild survived holding stdout/stderr, so
  the pipe-reader joins never returned. Timeout now kills the whole group
  (same escalation as validation), and unsandboxed eval runs get the same
  unsafe-env guard as every other spawn.
- **Eval-case file names were written unsanitized (audit X-N5).** The name
  comes from the CP-supplied assignment and went straight into
  `dir.join(name)`; a hostile `../../x` escaped the worktree. Names are now
  filtered to safe segment characters with traversal rejected.
- **A crash between agent exit and the final report could publish a false
  success (audit X-N6).** The durable early-completion record persisted
  `exit=0` before validation/eval ran; if the daemon died after a failed
  validation, startup recovery redelivered the provisional record and the CP
  marked the attempt `succeeded` (redelivery wins over re-running). A
  success is now recorded early only when no later stage can flip the
  verdict (no validation command, no eval cases); failures always record.
  Losing an unverdicted success is recoverable (reaper → retry), a false
  one was not.
- **`/metrics` task gauges and outcome counters were computed from the
  oldest 1000 tasks (audit X-C3).** `list_tasks()` caps at the oldest page,
  so past that size the counters froze and running-task alerting went blind
  to new tasks. Counts now come from a full-table GROUP BY and the duration
  histogram from the newest terminal window.
- **Prometheus label injection via node-supplied values (audit X-C4).** The
  `validation_outcomes_total` / `attempts_by_security_profile_total`
  series interpolated raw error codes / profile names — an authenticated
  node could forge arbitrary time series with an embedded newline. Labels
  go through the existing escaping helper now.
- **Keyset pagination silently dropped rows when the client requested more
  than the server cap (audit X-C5).** `next_cursor` was derived by
  comparing against the requested limit; a `?limit=2000` client received
  the capped 1000 rows, saw "page not full", and never fetched the tail. All
  list routes now compare against the effective page size.
- **A conversation turn could run unlogged (audit X-C6).** The user message
  was appended after task creation: a failed INSERT left a live agent
  answering a turn missing from history, so every later prompt silently
  omitted it. The message is persisted first, then the task is created and
  linked.
- **Concurrent consensus completions minted duplicate disagreement
  approvals (audit X-C7).** The dedup check was scoped to each member's own
  task id and ran outside any transaction, so the last two members finishing
  together both passed and both inserted. Check+insert is serialized under
  the write gate and scoped to the whole consensus group.
- **Fully-delivered event spools were never deleted — the node eventually
  bricked itself with `spool_full` (audit X-N1).** Every attempt left its
  `<attempt>.jsonl` behind forever while the global quota scan counted every
  file under the outbox root; once the accumulated history crossed
  `AGENTGRID_OUTBOX_QUOTA_BYTES` (default 1 GiB) each new `push` failed and
  every new attempt terminated `spool_full` until an operator wiped the
  directory by hand. Terminal attempts now discard their drained spool file
  (startup recovery never replays event outboxes of terminal attempts and a
  retry gets a fresh attempt id, so nothing deliverable is lost).
- **The in-flight guard leaked on the completion-redelivery path — later
  redeliveries of that attempt were dropped silently forever (audit X-N2).**
  The IN_FLIGHT entry was removed only at the normal end of the runner task,
  so the branch that redelivers an undelivered completion instead of running
  the agent never released it: after one failed redelivery, every future
  offer of that assignment hit the duplicate check and was discarded until
  daemon restart. The entry is now an RAII guard, so both the redelivery
  branch and a panicking runner release the slot.
- **A mirror clone killed mid-write bricked the repository cache until
  manual cleanup (audit X-N7).** The partial directory has no `HEAD`, so
  every subsequent attempt took the clone path and `git clone --mirror`
  refused the non-empty destination — permanent failures for that repo.
  `prepare_workspace` now detects the partial state and re-clones fresh.
- **An incompatible-protocol node showed `online` again on its next
  heartbeat (audit X-C1).** The handler set `degraded`, but the heartbeat
  UPDATE only keeps `revoked` sticky and overwrote status with the
  daemon-reported value — the node stayed schedulable despite the protocol
  gate. The beat's reported status is now pinned to `degraded`; the node
  returns online naturally once it reports a compatible protocol.
- **A workflow run could wedge in `plan_ready` forever if the process died
  between expanding steps and flipping status (audit X-C2).** The status
  flip ran outside the expansion transaction; on retry the re-inserted steps
  violated the unique index and surfaced as an opaque 500 every time. The
  flip is now a CAS inside the same transaction (`plan_ready → running`),
  which also makes a concurrent double-approve fail cleanly as 409.
- **`reject_assignment` left `tasks.assigned_attempt_id` pointing at the
  dead attempt** (residual from the WS-reject fix): terminal tasks are
  required to have no active attempt; now cleared like every other terminal
  path.
- **A workspace-finalize failure aborted the attempt with no completion and
  no cleanup (audit ND-3).** Both runner branches propagated the
  `finalize_workspace` error out of `run_attempt`, so no `report_complete`
  was ever sent — the CP kept the attempt `running` until the ack-deadline
  reaper marked it `lost` — and the worktree/branch leaked until the 24h
  prune. A finalize failure is now a reported terminal outcome:
  exit non-zero with `error_code=infrastructure_failed` (outranking
  agent/validation verdicts — without a finalized worktree there is no
  deliverable result), followed by the normal drain/report/cleanup path.
  `Ok(None)` (non-git or nothing to commit) remains a success.
- **Serialized startup recovery delayed the first heartbeat (audit ND-9).**
  `startup_recovery` ran before `spawn_heartbeat`: each pending durable
  completion redelivery can back off up to 20 rounds against a slow or down
  CP, so a long recovery kept the node silent past the 30s offline sweep and
  flapped it offline mid-recovery. Recovery now runs as a spawned background
  task — it is best-effort, and its one unsafe interleaving (a redelivered
  assignment re-running an attempt whose completion is still undelivered) is
  already guarded by the ND-4 completion-redelivery check.
- **ACP agent stderr bypassed secret redaction (audit ND-10).**
  `drive_acp_session` spawned the adapter with `Stdio::inherit()` on stderr,
  so anything the agent printed there landed raw in the daemon log — unlike
  every wrapper-path stream, which goes through the streaming redactor. The
  stderr is now piped and drained through the same masking `read_stream`,
  so agent stderr reaches the attempt's event stream (as `stderr` log
  events) with secrets masked.
- **Late ack after a lease revert was accepted as success (unfenced duplicate
  execution).** When the ack-deadline reaper reverted an assignment (attempt
  `cancelled`, task requeued) and the node's runner then started — e.g. after
  waiting on the concurrency semaphore past the 30s deadline — the CP's
  `ack_attempt` returned 200 for the `cancelled` attempt, the fencing token
  was never rotated and no cancel was flagged, so the stale holder executed
  the whole task alongside the new one with artifact uploads still accepted.
  Now: the CP rejects acks on `cancelled` attempts (404), the revert rotates
  the fencing token in the same CAS transaction (stale uploads 409), and the
  node treats a 404/409 ack as terminal — it drops the assignment before
  spawning the agent (network errors keep the historical best-effort
  semantics).
- **Unknown task `network_mode` reached `--network` verbatim.** An assignment
  carrying e.g. `network_mode: "host"` passed the CP's capacity gate (unknown
  modes rank as `none`) and the daemon executed `docker run --network host` —
  full host egress — while the egress-audit log printed
  `resolved_network=none`. Task-supplied modes are now clamped fail-closed to
  `none`; the operator `AGENTGRID_SANDBOX_NETWORK` env stays trusted
  (startup-validated; may name an egress-proxy network).
- **WS reconnect redelivery aborted wholesale for tasks without a
  `repositories` row** (repository `"*"` from webhooks, ad-hoc names): the
  redelivery query decoded the LEFT JOIN's NULL `git_url`/`default_branch`
  as non-optional and one such task failed the entire batch, stranding every
  unacked assignment of the node. Decodes as `Option` with empty-string
  defaults now, matching the assign path.
- **ACP attempts lost their tail events and artifacts forever.** The ACP
  branch of the runner never drained the durable event outbox after the
  session ended (the flusher died with `drive_acp_session`), so
  validating/eval-fail/account-rotation events written after the session
  stranded on disk permanently — startup recovery never replays event
  outboxes of terminal attempts. It also never uploaded `changes.patch` /
  `validation.log`: both were written, then destroyed by the workspace
  cleanup, so the documented upstream-patch fallback silently degraded.
  The branch now mirrors the wrapper: uploads + a pre-completion drain +
  pending-artifact reporting + a post-completion redelivery drain.
- **WS assignment reject (`ok=false`) was a no-op that looped forever.**
  The reject path called `complete_attempt` with a Fail transition on an
  still-`assigned` attempt — an invalid transition the handler swallowed,
  so the attempt sat assigned until the 30s reaper, got reassigned to the
  same node, and rejected again; the task never reached a terminal state
  (the node protocol doc promises "immediately failed"). New
  `reject_assignment` store path applies the legal NodeLost pairing
  (attempt `lost`, task `failed`) with the node counter decremented and an
  audit row carrying the reject reason.
- **GitHub webhook deliveries were not deduplicated.** Delivery is
  at-least-once (response lost after commit, timeout, manual redelivery),
  and every replay minted a fresh task — duplicate full agent runs for the
  same issue/CI failure/PR. The delivery GUID (`X-GitHub-Delivery`) is now
  recorded in a `webhook_deliveries` table (INSERT OR IGNORE) before any
  task creation across all three webhook handlers.
- **A redelivered assignment with an undelivered completion re-ran the whole
  agent.** The in-flight redelivery guard is dropped the moment
  `run_attempt` returns, but a completion that never got a CP ack (outage
  longer than the retry budget, or a non-retryable 4xx) leaves the CP
  showing the attempt running — the redelivery then passed the guard and
  started a duplicate execution instead of replaying the durable
  completion the node already held. `dispatch_batch` now redelivers the
  recorded completion for such attempts (shared with startup recovery via
  `redeliver_completion`) instead of spawning a second runner.
- **The WS control session stalled under over-subscription.** The
  assignment handler awaited `dispatch_batch`, which waits on the
  concurrency semaphore per assignment — with a full node the session
  stopped answering pings and processing Cancel/ConfigUpdate until the
  connection flapped, then re-entered the same stall after reconnect. The
  dispatch is now spawned; the receipt ack confirms delivery only and the
  runner's HTTP ack stays the authoritative "started" signal.
- **Cancel/timeout killed the `docker run` client, not the container.**
  SIGTERM is proxied, but the 10s SIGKILL escalation kills the client
  without forwarding and the client's death leaves the container running —
  writing into the worktree being finalized/cleaned and burning CPU until
  the next startup orphan sweep. Sandbox containers now carry a
  deterministic per-attempt `--name`, and all cancel/timeout paths
  best-effort `rm -f` it after killing the client.
- **Fenced-off artifact uploads were retried forever with a doomed token.**
  A 409/412 (stale writer) left the artifact staged "for retry", but the
  only retry is startup recovery, which sends an empty fencing token —
  guaranteed to fail again for token-bearing attempts, once per restart
  until the 24h orphan reaper. Fencing rejections are now terminal: the
  staged copy is dropped immediately.
- **A workflow step wedged `running` forever when its task link was
  missing.** The pending→running claim commits in its own transaction and
  the task create/link run outside it; a crash in between (or one transient
  DB error) left a Running step with `role_runs.task_id` NULL — a state no
  tick branch could progress or terminate, pinning the whole run in
  `running` with the 5s ticker spinning on it. The tick now detects a
  Running step with no task link and resets it to Pending for a clean
  re-claim.
- **Concurrent uploads of the same artifact shared one tmp path**
  (`tmp.upload`): a retried upload racing the original could swap bytes under
  the other writer's committed sha row (and the second rename fails on
  Windows). The tmp name now carries a per-write uuid suffix.

## [v0.3.5] — 2026-08-22

### Fixed

- **Flaky unit tests under the CI churn job (high-parallelism, repeat).**
  Three independent races each surfaced at 16-thread test stress:
  - `read_stream` env-var race — `read_stream` captures
    `AGENTGRID_MAX_LINE_BYTES` at start, and `read_stream_caps_oversized_line`
    temporarily sets it to `16`. A test starting in that window built a 16-byte
    redactor and truncated its own lines, so `"hello"` never reached the raw
    mirror (`read_stream_mirrors_raw_output` flake) or a secret was cut before
    masking. Serialized the mutator and the tests whose assertions depend on
    the knob behind a `READ_STREAM_ENV_LOCK` (mirrors the existing
    `sandbox::tests::ENV_LOCK`).
  - `mcp`/`profiles`/`ingest` dummy-server RST race — the test TCP servers
    wrote the response and dropped the connection **without reading the
    request**; closing a socket with unread request bytes races a TCP RST that
    discards the queued response on the client side, making
    `mcp_servers_payload` return `Null` and the `unwrap` panic. They now read
    the request before replying and half-close the write side.
  - `unsafe_guard_strips_unset_env_when_unsandboxed` /
    `unsafe_guard_keeps_env_with_override` mutated the process-global
    `AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX` outside the existing sandbox
    `ENV_LOCK`; both now take it.
  Stress-verified: the full `node-daemon` suite ran 10× at `--test-threads=16`
  with 0 failures (previously reproducible flakes within 2–3 passes).
- **Stale `Cargo.lock` broke the nightly churn job.** The committed lock pinned
  the workspace crates at `0.3.2` while the manifests had moved to `0.3.4`, so
  `cargo test --workspace --no-run --locked` failed with
  "cannot update the lock file". Regenerated the lock (`cargo update
  --workspace`); the bump touches only the workspace members, no dependency
  churn. `--locked` full-workspace compile verified green.
- **Sandbox spawn now clears the image ENTRYPOINT**: `docker run` passes the
  explicit `<program> <args>` after the image ref, but an image ENTRYPOINT
  (the GHCR node-daemon image ships the daemon as its ENTRYPOINT) makes docker
  exec `<entrypoint> <program> <args>` — the daemon starts inside the sandbox
  and dies, so the node-daemon image could not serve as a sandbox base. The
  spawn head now adds `--entrypoint ""` so the explicit command wins for any
  image (regression test `sandbox::tests::docker_clears_image_entrypoint`).

- **`deploy/install-control-plane.sh`: broken echo at the tail.** The final
  hint line shipped as one `echo` with a stray escaped quote (mangled by the
  CRLF round-trip through PowerShell during the Windows lab) and printed
  garbage; split back into two clean `echo` lines (`bash -n` clean).
- **Scheduled CI (Miri) red since 08-17.** The three
  `external_provider_fail_closed_*` tests in `agentgrid-common` spawn a
  subprocess (`false`/`true`/missing binary); a Miri nightly update stopped
  shimming `posix_spawnattr_init`, so the scheduled UB job died on
  "unsupported operation: can't call foreign function". The spawning tests
  are now `#[cfg_attr(miri, ignore)]` — they exercise the fail-closed error
  path, not UB, and keep running under the normal test job. Past that crash
  the job still blew its 30-min timeout: the state-machine proptests run
  ~256 cases each, 2.5-4 min under Miri interpretation. The Miri job now
  sets `PROPTEST_CASES=25` (UB checking needs code-path coverage, not
  statistical power) and a 40-min timeout.

### Changed

- **Windows deploy checklist, track B (podman sandbox)** — verified on the
  WSL2 lab (podman 5.7.0, rootless): replaced the GHCR-image alternative with
  the local sandbox-image recipe (ubuntu:24.04 + adapter binaries, `ENTRYPOINT
  []`; mind the exec bit before `COPY` — a 644 binary makes the in-image
  adapter probe fail), documented the rootless drop-in
  (`RuntimeDirectory`/`XDG_RUNTIME_DIR`, subuid/subgid), recorded the measured
  cold start (raw container start 0.47–0.9 s, end-to-end task ~0.7 s) and
  marked the track complete. Also fixed the stale `ghcr.io/…/agent-sandbox`
  default-image reference in `docs/deploy/sandbox-benchmark.md` (the actual
  default is `ubuntu:24.04`).
- **Windows deploy checklist, track C (Hyper-V VM) + rollback.** Verified on
  the lab VM: rootful podman inside the hardened node unit needs an explicit
  relaxation chain (`ReadWritePaths=/var/cache/containers`,
  `ProtectControlGroups=false`+`Delegate=yes`, `PrivateDevices=false`,
  `ProtectHostname=false`, `NoNewPrivileges=false`) on top of track B's
  drop-in, and node/CP binaries >= v0.3.4 (the v0.3.2 probe without
  `--network none` false-reports "adapter missing in sandbox image" under the
  hardened unit). Acceptance re-run: mock task `succeeded` in a digest-pinned
  container (`network none`, label stamped, `--rm` clean), `ag nodes doctor`
  OK. The rollback section was audited against live host state and then
  executed in full (WSL distros unregistered, VM removed, Hyper-V disabled,
  Docker Desktop uninstalled); reconstruction stays documented in the
  checklist.

## [v0.3.4] — 2026-08-17

### Fixed

- **Idempotent assignment dispatch**: the redelivery above exposed that a
  redelivered assignment whose ack had not landed yet would start a SECOND
  runner for the same attempt, interleaving two event streams (the outbox
  e2e caught it: 268 events where 200 were expected). Delivery is
  at-least-once; `dispatch_batch` now drops duplicates via a process-wide
  in-flight attempt set.
- **Sandbox adapter probe used the default bridge network**, dragging in
  netavark/nftables setup that fails under rootless podman on minimal
  service environments (no user session / WSL2 kernel) — a healthy image
  reported as missing and the node degraded. The probe now mirrors the
  sandbox itself (`--network none`).
- **Orphan-container cleanup was a no-op**: `--filter agentgrid.node=<id>`
  was missing the `label=` prefix and is rejected by both docker and
  podman.
- **WS assignment redelivery on reconnect (ws_node_survives_cp_restart
  flake)**: a best-effort push to a connection that died mid-delivery
  silently succeeds at the channel layer, leaving the attempt `assigned`
  and never acked until the ack deadline — and the reconnect path only
  handed out still-QUEUED work. On registration the CP now redelivers the
  node's unacked assignments over the fresh socket (original fencing
  tokens preserved, `ws_redeliveries` counter in /metrics).

## [v0.3.3] — 2026-08-16

### Added

- **`GET /v1/nodes/{id}`** — the control plane's view of a single node. Was
  documented in the OpenAPI and consumed by `ag node doctor`, but the route
  had shipped DELETE-only (405). Found end-to-end while deploying the v0.3.2
  lab node; includes an API test (view + 404 on unknown id).

### Fixed

- **install-control-plane.sh set a node-daemon env var**: the CP reads
  `AGENTGRID_DB` (default is a RELATIVE `control-plane.db` — under systemd,
  cwd=/ means EACCES creating the lock file in /). The unit now sets
  `AGENTGRID_DB=<data>/control-plane.db`.
- **install-node.sh verified the wrong binary names** (`/usr/local/bin/mock`
  instead of `adapter-mock`) and aborted a fully successful install.
- **run-disk-full e2e could never pass**: the spool_full terminal error
  event carried only a human `error` string, while the e2e asserts on a
  machine-readable `error_code`/`event` key — the assert could not match
  even when the latch fired and the completion already reported
  `error_code=spool_full`. The event now carries `"error_code":
  "spool_full"` alongside the human message.
- **pr-validation "Docker Build" referenced a Dockerfile that was never
  committed** (`Dockerfile.node-daemon-musl`) — the check failed on every PR
  since the workflow landed.
- **WSL2 deployment note**: the shipped systemd sandbox (ProtectSystem /
  PrivateTmp / ReadWritePaths) is incompatible with the WSL filesystem
  (EROFS/EACCES); documented drop-in that relaxes it (the WSL2 VM boundary
  provides isolation instead).

### Dependencies (the queued dependabot set)

- sqlx 0.8.6 → **0.9.0** — new compile-time dynamic-SQL audit: all nine
  dynamic query sites audited and wrapped in `AssertSqlSafe` (clauses are
  compile-time constants; every value is a bound parameter).
- tokio-tungstenite 0.24 → **0.29** — `Message::Text` now takes `Utf8Bytes`;
  all send sites converted.
- OpenTelemetry stack 0.27 → **0.32** (opentelemetry, sdk,
  opentelemetry-prometheus) + prometheus 0.13 → **0.14** — landed as one
  consistent set: individually each bump leaves duplicate crate versions in
  the graph. `Resource::new` became `pub(crate)`; migrated to the builder
  API.
- tower-http 0.6 → **0.7**, clap 4.6.6, thiserror 2.0.20, libc 0.2.189.
- Actions: trivy-action 0.36, cosign-installer 4.1.2, codeql-action pin,
  docker/setup-buildx-action v4.

## [v0.3.2] — 2026-08-16

### Fixed (release pipeline — unblocks the v0.3.1→v0.3.2 gap)

- **Cross-test 429 flake in the events ingest suite.**
  `events_rate_limit_throttles_one_node` configured its tiny throttle budget
  via `std::env::set_var("AGENTGRID_EVENT_RATE_MAX", "2")` (window 3600s).
  Env is process-global while every `#[tokio::test]` builds its own
  `AppState` — any test constructing a state in that window inherited the
  poisoned limiter and 429'd on its third ingest. This is what killed both
  v0.3.1 release builds (`events_ordered_by_global_ingest_cursor_across_attempts`:
  `429 != 200`) and the nightly stress run
  (`ingest_id_monotonic_under_concurrent_ingestion`). The env save/set/restore
  is gone: `AppState::set_event_rate_limits(max, window)` +
  `EventRate::with_limits` let the test throttle exactly its own state.
- **CI `test` step fails on stable: `--report-time` is nightly-only.**
  Dropped the flag (surfacing slow tests can return under a nightly job).
- **Miri job: workflow-yaml tests write temp files, forbidden under Miri
  isolation.** `cargo +nightly miri test -p agentgrid-common` now runs with
  `-Zmiri-disable-isolation`.
- **Skill-bundle nightly job died on its own skip path.** The step shell is
  `bash -e`, so the script's exit 77 ("no AG_REMOTE_* configured") killed the
  step before the `rc` check could convert it into a skip. Guarded with
  `|| rc=$?`.
- **Release image job was unrunnable: `docker/build-push-action` pinned to
  a commit SHA that does not exist on the action's repo (broken dependabot
  bump residue — "unable to find version"). Re-pinned to the real v7.3.0
  commit (53b7df96c91f9c12dcc8a07bcb9ccacbed38856a).

- **Release smoke test could never pass: `agentgrid-node-daemon --version`
  was not handled.** The daemon parses no CLI args, so the smoke step's
  `--version` flag was silently ignored and the binary booted for real —
  probing adapters, pruning workspaces, then exiting 1 on a failed enroll
  against a nonexistent control plane. `--version`/`-V` now print the
  version and exit before any startup side effects.

- **`adapter-claude --version`**: like the daemon, adapter-claude parsed no
  `--version` and instead started as an agent (exit 127 trying to spawn
  `claude` on the runner). Now prints the version and exits (mirrors the
  existing adapter-opencode behavior).
- **GHCR image tag must be lowercase**: `github.repository_owner`
  ("Fongobongo") was interpolated verbatim into the image tag; GHCR
  rejects uppercase repository names. The owner is lowercased when the
  tags are derived.

- **aarch64 smoke could never execute**: the smoke step ran the
  cross-compiled aarch64 binaries directly on the x86_64 runner
  ("Exec format error"). The step now executes only runner-arch (x86-64)
  binaries and asserts the ELF architecture of foreign-arch builds.

- **Flaky aarch64 cross build (GLIBC mismatch)**: rust-cache restored
  host-compiled build scripts into the cross container, whose older glibc
  rejected them (`version GLIBC_2.28 not found`). Same commit passed and
  failed depending on cache state; the release workflow now builds clean
  (no rust-cache).

- **Release asset layout**: the flatten step copied same-named binaries from
  all three targets into one directory (silent overwrites) and its
  SHA256SUMS namespacing collapsed to a single file — the step had never
  run before this release. Replaced by per-target tarballs
  (`agentgrid-<target>.tar.gz`, each with its own SHA256SUMS inside);
  v0.3.2's release page carries the three tarballs + SBOM + notes, and the
  image job's incidental `.dockerbuild` artifacts are filtered out of the
  release job's download (they made it fail with persistent download
  retries).

- **`GET /v1/nodes/{id}` was documented and CLI-consumed but never
  implemented** — `ag node doctor` (report-only diagnostics) got a 405
  because the route shipped DELETE-only. Handler added (single-node view,
  404 on unknown id); found end-to-end while deploying the v0.3.2 lab node.
- **install-control-plane.sh set a node-daemon env var**: the CP reads
  `AGENTGRID_DB` (default is a RELATIVE `control-plane.db` — under systemd,
  cwd=/ means an EACCES trying to create the lock file in /). The unit now
  sets `AGENTGRID_DB=<data>/control-plane.db`.
- **install-node.sh verified the wrong binary names**: the post-install
  check looked for `/usr/local/bin/<adapter-id>` instead of
  `adapter-<adapter-id>` and aborted a fully successful install.
- **WSL2 note (lab deployment)**: the shipped systemd sandbox
  (ProtectSystem=strict / PrivateTmp / ReadWritePaths) is incompatible with
  the WSL filesystem (EROFS/EACCES); deploy via a drop-in that relaxes
  namespace sandboxing — the WSL2 VM boundary provides isolation instead.

### Known issues (not gating)

- **`run-disk-full.sh` e2e**: the chatty-task run reaches `failed` but no
  `spool_full` error event is observed — the fail-closed latch path
  (event_sink → outbox) does not fire as scripted since the attempt-runner
  refactor / WS transport landed. Pre-existing on master (the job had not
  run green since the CI test step went red); needs a focused
  investigation, tracked for the next patch release.
- **Supply chain.** `issues: write` added so audit-check can file its
  tracking issue instead of dying with "Resource not accessible by
  integration". RUSTSEC-2024-0437 (protobuf 2.28 recursion crash, pulled by
  `prometheus` for metric encoding only — no untrusted protobuf parsed in
  this workspace) is a documented ignore in `deny.toml` + the audit job.
  Three gitleaks false positives (e2e JWT test fixture + two synthetic
  test secrets in historical commits of the secret-redaction tests) are
  fingerprinted in `.gitleaksignore`.

### Removed (dead 0.4 spike surface)

- **Redis cache layer.** `crates/control-plane/src/cache.rs` + `cache/redis.rs`
  (and the `redis` Cargo dependency) were an orphaned stub from an Aug-7
  "0.4 production" spike: `mod cache` was never declared in `lib.rs`, no
  production path referenced it, the file's own docstring said "a stub
  demonstrating API compatibility". Removed (continues the k8s/Helm drop from
  commit 9284072 — the same spike's heavy-load surface). Release binary loses
  the bundled redis dependency. `docs/plans/deploy-k8s.md` (Helm install guide)
  was orphaned with it — deleted. `docs/plans/0.4-production-ready.md`
  (PostgreSQL + Redis + k8s design) marked DEPRECATED/SUPERSEDED: it ran
  against agentgrid's hard constraints (SQLite-only, no external DB/NFS,
  no required Docker/Node runtime, single control-plane instance).

### Fixed (capacity-pressure gate writer)

- **`max_rss_mib` heartbeat writer (Plan 2.14 follow-up).** The capacity-pressure
  gate reads `nodes.max_rss_mib` and rejects assignments when
  `active_rss + forecast*256` exceeds it, but the column had no writer — it
  stayed pinned to the schema default (1024 MiB) forever. An operator on a
  small host (Termux 256, RPi 512, a constrained VM) could never lower the
  gate, and real OOM pressure slipped through. Now the node declares its own
  ceiling in `AGENTGRID_MAX_RSS_MIB` (MiB); the heartbeat sends
  `HeartbeatRequest.max_rss_mib`, and the CP UPDATE writes it only when
  `> 0` (legacy / unset nodes keep the row value). `docs/deploy-termux.md`
  points the script's `max_rss_mib = 256` default at the new knob. Test:
  `heartbeat_max_rss_mib_overrides_schema_default_only_when_set`.

### Added (agentgrid toolkit + transport validation + ops)

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
- docs/plans/0.3-websocket-and-scale.md — plan file; removed from the tree after completion (all items shipped by v0.4.0; git history retains it)
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
