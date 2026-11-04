# Response to the external code review

Reviewed document: `agentgrid code review`, base commit `5e5372d` (branch `main`),
16 ranked findings plus an architecture/maintainability section and a test-gap
list. Scope of the review was read-only; this document is the project's answer.

Response is current as of `855df70` plus the 2026-11-04 hardening batch
(`open_source` in the artifact spool, production `/metrics` fail-closed).

**Result: 11 of 16 findings are closed. Five are mitigated with an explicit,
documented residual risk.** "Closed" means the described attack no longer works
and a regression test covers it. "Mitigated" means the exposure is narrowed or
the control is now honest about what it enforces, but part of the original risk
remains by design. Nothing below is marked closed on the strength of a comment.

## Summary

| # | Severity | Finding | Status | Shipped in |
|---|---|---|---|---|
| 1 | CRITICAL | Host code execution via agent-written `.git` | Closed | `4cb4eab` |
| 2 | HIGH | Daemon follows symlinks in the worktree | Closed | `4cb4eab`, 2026-11-04 batch |
| 3 | HIGH | bwrap exposes all host secrets; `restricted` not isolated | **Mitigated** | `9f16ed8` |
| 4 | HIGH | Command guard never blocks anything | **Mitigated** | `9f16ed8` |
| 5 | MEDIUM-HIGH | Secrets in the `docker run` argv | Closed | `4cb4eab` |
| 6 | MEDIUM | Unescaped `Host` in a root-run join script | Closed | `38c835c` |
| 7 | MEDIUM | RBAC/sessions fixed for 12 h; no user lifecycle | Closed | `a5c9b2d` |
| 8 | MEDIUM | Operator role reads secrets through GET | Closed | `3617d2b` |
| 9 | MEDIUM | Revoking a node leaves its WebSocket open | Closed | `38c835c` |
| 10 | MEDIUM | Webhook dedup drops a delivery | Closed | `38c835c` |
| 11 | MEDIUM | Peer relay: plain HTTP, whole CP API | **Mitigated** | `4ddbf1f` |
| 12 | MEDIUM | Login rate limiting keyed only on username | Closed | `95283f9` |
| 13 | LOW-MEDIUM | Sync HA commits locally before quorum ack | **Mitigated** | `4ddbf1f` |
| 14 | LOW | Public endpoints: `/metrics`, `/health/ready`, body limit | Closed | `95283f9`, 2026-11-04 batch |
| 15 | LOW | Argument injection in the git URL | Closed | `38c835c` |
| 16 | LOW | Other hardening (sandbox defaults, WS caps, cookie, docker user) | **Mitigated** | `a5c9b2d` |

Supporting commits: `9f20c2b` (secret-scan fixtures), `4a406d2` / `855df70`
(e2e passwords after the new 12-character minimum), `5cca007` / `87319c1`
(test stability).

## Closed findings — what actually shipped

**1 — host-side git.** The daemon records the git dir when it creates the
worktree (`recorded_git_dir`), passes `--git-dir`/`--work-tree` explicitly on
every host call, and refuses a worktree whose `.git` no longer points there
(`verify_worktree_git_dir`). Git runs with `GIT_CONFIG_NOSYSTEM=1`, an empty
global config, a `core.hooksPath` that never exists, `core.fsmonitor=false`,
and `--no-verify` on commit. `git/tests.rs` plants a replacement `.git` with
an `core.fsmonitor` hook and asserts it never runs.

**2 — symlinked outputs.** `write_owned` / `create_owned` remove the entry and
recreate it with `O_EXCL | O_NOFOLLOW`; `stage` refuses a non-regular source,
copies through `take(max+1)`, and now (2026-11-04) makes its decision on the
opened descriptor — see "Post-review hardening" below. Tests cover a symlinked
`changes.patch`, an oversized source, a hard link, and a swap between the two
path lookups.

**5 — docker argv.** `--env KEY=VALUE` became `--env KEY`; docker copies the
value from the client's own environment. The egress-proxy URL (which can carry
`user:pass@`) follows the same rule.

**6 — join script.** `Host` is validated against `[A-Za-z0-9.:-]` plus IPv6
brackets, the substitution is single-quoted, an unusable `Host` is a `400`
naming `AGENTGRID_PUBLIC_URL`, and an `http://` script carries a warning.

**7 — session lifecycle.** `verify_token` reads the role from `users` on every
request; a missing row or a missing `role` claim is not admin. Added
`DELETE`/`PATCH /v1/users/{name}`, per-user `sessions_valid_after`, and a
12-character password minimum.

**8 — operator RBAC.** `OPERATOR_READ` is an allow-list; a new GET is admin-only
until declared. `OPERATOR_READ_DENIED` pins the two credential-bearing routes.
A unit test walks **every GET in `docs/openapi.yaml`** and fails if a path is
in neither list — it found one documented route with no handler.

**9 — WebSocket revoke.** `revoke_node` calls
`ws_registry.close(node_id, WS_CLOSE_UNAUTHORIZED, ...)`; the upgrade path
rejects revoked credentials. Covered by `tests/ws.rs`.

**10 — webhook dedup.** Dedup row and task creation are one write transaction.
Adapter resolves from repository config, repository uses `owner/name`, and the
hand-rolled HMAC was replaced with `hmac::Hmac<Sha256>::verify_slice`.

**12 — login limiting.** A per-source-address gate (30/min, `X-Forwarded-For`
only from `AGENTGRID_TRUSTED_PROXIES` peers) sits in front of a per-account
*failure* counter that a correct password clears, so a spent budget cannot
lock out the real user. Unknown usernames pay for a dummy Argon2 verify. Both
maps are capped at 4096 keys.

**14 — public endpoints.** `/health/ready` caches its disk+DB verdict for one
second; the global body limit is the largest non-artifact body, raised to
`artifact` only on the two upload routes; `/metrics` has a bearer token and
(2026-11-04) is **closed by default in production**. See below.

**15 — git URL.** A leading `-` and any `::` transport form are refused,
metacharacters/whitespace are rejected, and `git clone` uses `--` before
positional arguments.

## Mitigated findings — residual risk

These five are the honest answer to "are all 16 fixed?". Each names what is
still true, where, and what would be needed to close it.

### R1 (finding 3): bwrap still bind-mounts the host root read-only

`sandbox.rs:870` is still `--ro-bind / /`. `bwrap_masked_paths()` puts an
empty `--tmpfs` over exactly three subtrees: `AGENTGRID_DATA_DIR`,
`workdir.parent()` (the workspace root) and `$HOME`. That hides the node
credential, the sealed-env key, `~/.ssh` and sibling worktrees, which was the
concrete exposure in the finding. It does **not** hide anything else the daemon
user can read — `/etc` secrets, `/run/secrets`, other users' homes, mounted
volumes. If `HOME` is unset, the home mask is skipped entirely.

The other half of finding 3 is closed: `restricted` under bwrap is refused at
startup and for a task-supplied mode, and `effective_egress_isolated` returns
false for `(Bwrap, "restricted")` instead of claiming isolation.

To close: stop binding `/` and build a curated root (`/usr`, `/lib*`, `/bin`,
filtered `/etc`, the adapter binary, the worktree), as the review recommended.

### R2 (finding 4): the command guard is post-hoc by design

A `guard_block` now **does** end the attempt: `process_supervisor.rs` latches
the refusal, kills the process group, sets `kill_reason = "guard_blocked"`, and
`attempt_runner.rs` keeps `guard_blocked` terminal in both the retry loop and
`error_code`. Original events are preserved in the transcript; matching moved
from substring to argv tokens with wrapper skipping and newline separators; the
reason distinguishes denylist from not-allowlisted. `docs/openapi.yaml` and
`docs/agent-ops.md` say it is post-hoc.

But `decide()` still runs on the `tool_call` event the adapter emits **after or
while** the tool runs. A guarded command gets one execution before the kill.
There is no enforcement at the adapter permission hook (`permission_interception`)
or a wrapper shell in the sandbox. To close: intercept before exec.

### R3 (finding 11): the peer relay has no transport encryption

The API surface is closed: `EdgeConfig::allowed_paths` is `["/v1/node/"]` on
the relay, the 404 happens before any body is read, `path_refused_total` counts
refusals, and a non-loopback listen **refuses to start** without
`AGENTGRID_RELAY_ALLOW` (`relay.rs:262`). A loopback listener without an
allow-list logs a warning.

The listener itself is still plain HTTP. Node credentials and any JWT a client
sends over the relay cross the peer link in clear text. `docs/runbook-transport.md`
and `docs/agent-ops.md` state that the peer→relay leg requires WireGuard, an
ssh tunnel or a trusted network segment. To close natively: TLS/mTLS on the
relay listener.

### R4 (finding 13): sync HA has no client idempotency or group commit

`4ddbf1f` added `suspend_writes()` the moment a quorum acknowledgement fails,
`assert_still_writable()` immediately before `COMMIT` (a lease that expires
mid-transaction rolls back), and `docs/ha-deployment.md` now states that an
error means "not acknowledged", not "not applied".

Still open: no client-supplied request ids, so a client retry can create a
duplicate if the leader stayed writable; no group commit, so the single
`WRITE_GATE` permit is held across the replication round trip. These were the
review's two optional items and are deliberate scope boundaries.

### R5 (finding 16): docker still runs as root in the container by default

`--user` is emitted only when `AGENTGRID_RUN_AS_USER` is set and passes
validation; `sandbox.rs:2464` asserts that no `--user` appears without
configuration. The worktree remains bind-mounted read-write. A blind non-root
default would break every attempt, because the bind-mounted worktree is owned
by the daemon user; startup warns and `docs/agent-ops.md` documents the setup.

The other three sub-items of finding 16 **are** closed: an unknown
`AGENTGRID_SANDBOX` is a startup error (production also refuses `none`), both
WebSockets cap messages at 1 MiB and frames at 256 KiB, and the session cookie
is `Secure` automatically under TLS or `AGENTGRID_ENV=production`.

## Post-review hardening (not among the 16)

Two gaps were found by re-auditing our own fixes on 2026-11-04.

**Artifact staging TOCTOU.** `stage` called `symlink_metadata(path)` and then
`File::open(path)` — two lookups in an agent-writable directory, and `open`
follows symlinks. A still-running agent or a background child could replace
`changes.patch` in that window and have `credential.json` staged as an
artifact. `open_source()` now opens with `O_NOFOLLOW` on unix and decides
(regular file, single hard link) from the `fstat` of the descriptor that is
actually read, so a later swap at the path changes nothing. A hard link is
refused explicitly: `ln <data_dir>/credential.json <workdir>/changes.patch` is
not a symlink. The source descriptor is also pinned **before** the spool
`.tmp` or attempt directory is created, so a refused source leaves nothing
behind. The pre-stat was split into `stage_from_stat`: a test can pass the
metadata captured before the swap and run the whole pipeline, which models the
race deterministically instead of relying on thread timing. Two regression
tests cover the swap and the hard link. `crates/node-daemon/src/artifact_spool.rs`.

Platform note: Windows has no stable `O_NOFOLLOW` or hard-link count in
`std` (`MetadataExt::number_of_links` is gated behind `windows_by_handle`).
There the path is re-stat'ed **after** the open — conservative, because the
descriptor is already pinned and a race can only refuse a good file, never
accept a bad one. Windows cannot detect a hard-linked source. Unix is the
target for the bwrap/docker sandbox threat model.

**`/metrics` public by default in production.** Finding 14 asked for a closed
endpoint; `95283f9` added `AGENTGRID_METRICS_TOKEN` but left the default open,
so an exposed control plane still gave anonymous scrapes every node name and a
full-table aggregation. With `AGENTGRID_ENV=production` and no token, the
endpoint now returns `401` with `WWW-Authenticate: Bearer`, and boot logs a
warning naming the variable. A configured token opens it on any host; clearing
the token in production closes it again rather than reverting to public. A dev
host stays public. `docs/monitoring.md` documents the behaviour; the production
compose E2E asserts both halves — an anonymous scrape gets `401`, one with the
token still returns the transport gauges.

## Test gaps from the review

All six named gaps now have coverage:

| Review gap | Coverage |
|---|---|
| Hostile worktree (planted `.git`, hooks, `fsmonitor`, symlinked outputs) | `crates/node-daemon/src/git/tests.rs`, `artifact_spool.rs` tests |
| Guard actually stops a command; bypass shapes | `process_supervisor.rs`, `command_guard.rs` tests |
| `revoke_node` closes a live WebSocket | `crates/control-plane/tests/ws.rs` |
| Webhook retried after a failed `create_task` | `routes/webhooks.rs` tests |
| RBAC matrix over every registered route | `auth.rs` openapi-driven test |
| bwrap hides data dir and sibling worktrees | `sandbox.rs:2954` and following tests |

## Verification

- **Linux (host1, detached worktree at `855df70` + this patch):** `cargo test -p agentgrid-node-daemon --bin agentgrid-node-daemon -- artifact_spool` — 10/10, including the two `#[cfg(unix)]` regressions (`a_source_swapped_between_the_stat_and_the_open_is_refused`, `stage_refuses_a_hardlinked_source`); `maintenance::tests` — 2/2; `--test api -- metrics` — 3/3; `cargo check --workspace --all-targets`, `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` clean.
- **Windows:** `artifact_spool` — 7/7 (the two unix-gated tests are correctly skipped), `maintenance::tests` — 2/2, `--test api -- metrics` — 3/3, clippy `-D warnings` clean. This covers the `#[cfg(windows)]` post-open re-stat branch; Windows still cannot detect a hard-linked source.
- **CI on `855df70`:** push run `41716122660`, full `workflow_dispatch` `41725850383` (41m17s), scheduled `41812863864` — all green. The full matrix includes Miri, race/concurrency stress, musl/aarch64 E2E, two-host E2E, real-agent E2E, unix-only guard tests, CodeQL, Supply chain, HA chaos, and the OpenAPI contract test (16 passed).
- **Static gates:** clippy `-D warnings` first, `cargo fmt --all --check` last — CI's format check runs first, so a formatting drift hides every other failure.
- **Visibility caveat:** the new `#[cfg(unix)]` spool tests and the `O_NOFOLLOW` path do **not** execute on Windows. The Linux run above and the Linux CI matrix are the proof; a Windows-only green run is not.

## Out of scope

The review's "Architecture and maintainability" section is not a security
finding and was not addressed as part of this response: ~109k lines, several
files over 1,500 lines, process-global env reads on hot paths, long
"Plan/Audit" comment histories, `CreateTaskRequest` built field by field,
a second `constant_time_eq` in `ha/routes.rs`, and the toolchain pin
(`rust-toolchain.toml` 1.99.1 vs `rust-version = "1.94"`). All remain open
as maintainability work. The review's claim that "a typed state machine would
have caught findings 1 and 2" is fair and is the strongest argument for that
refactor.
