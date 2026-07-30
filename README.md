# agentgrid

Distributed orchestrator for coding agents. A control plane dispatches tasks to
node daemons, each running an LLM agent adapter (Claude Code / Codex / OpenCode)
in an isolated git worktree. SQLite WAL on local disk, single active
control-plane instance.

> Status: MVP 0.1 — see [CHANGELOG.md](CHANGELOG.md).

## Feature maturity

| Area | Status | Notes |
|------|--------|-------|
| tasks / nodes / attempts / events / Git worktrees / adapters / artifacts | **stable** | core lifecycle, cross-node isolation, fencing tokens, retention |
| auth (JWT, setup-token bootstrap), CLI (`ag`), web UI shell | **beta** | usable, APIs still settling pre-1.0 |
| workflows (DAG / schedule / run projection) | **experimental** | gate behind an opt-in before relying on it |
| ACP gateway / Telegram gateway | **experimental** | separate binaries, not in the minimal release by default |
| skills / profiles / MCP registry | **experimental** | operator registry, semantics may change |
| schedules / plan expansion / zeroshot / context provider | **experimental** | subject to change |
| Docker/Podman sandbox backend | **beta** | the worktree is **not** a security sandbox — run untrusted agents in the Docker sandbox with a restrictive network/secrets policy |

Untrusted agents must run under the Docker/Podman sandbox with `permission_interception` and a network/secrets policy. A plain git worktree is **not** isolation against hostile code — see [`docs/decisions/threat-model.md`](docs/decisions/threat-model.md).

## Architecture

- **control-plane** (Axum + SQLite): task/attempt state machine, scheduler,
  node long-poll assignment, idempotent event ingest, artifacts, auth (JWT).
- **node-daemon**: long-poll loop, adapter subprocess per attempt in its own
  worktree + process group, streams stdout/stderr as events, reports completion.
- **adapters**: `mock` (no LLM), `claude`, `opencode` — translate the agent's
  JSON events into the agentgrid contract.
- **cli** (`ag`): submit/inspect tasks, list nodes, mint tokens, run server.
- **web**: TypeScript UI (Vite + React) served by the control plane.

## Quickstart (Docker)

Images are prebuilt as `ag-cp:test` / `ag-node:test` (or `docker compose build`).
Bring the stack up — this bootstraps a user, mints node enrollment tokens, and
writes `deploy/compose/.env`:

    ./deploy/compose/up.sh
    docker compose up -d

Control plane: http://127.0.0.1:7800. `up.sh` generates a random admin
password and prints it once (no baked-in `admin/changeme`). Two node daemons
(mock adapters) come online; submit a task:

    export AGENTGRID_SERVER=http://127.0.0.1:7800
    ag login admin <password-printed-by-up.sh>
    ag run <repo> "your prompt here" --adapter mock

Tear down: `./deploy/compose/down.sh` (or `docker compose down`).

> This is the **demo/eval** path: `docker-compose.demo.yml` makes the control
> plane reachable on the loopback address and runs mock adapters. For a
> **production** single-host install use the systemd installers instead —
> `deploy/install-control-plane.sh` (binds `127.0.0.1:7800` by default; pass
> `--listen 0.0.0.0` only with TLS) and `deploy/install-node.sh` (creates an
> unprivileged `agentgrid` user, installs a hardened systemd unit, and scrubs the
> enrollment token after first connect). Both are idempotent.

## OpenCode node (optional)

The `opencode` CLI is operator-provided by default. To bake it into a portable
node image:

    docker build --build-arg OPENCODE_VERSION=1.17.16 -f Dockerfile.node-daemon -t ag-node-opencode .

Then set `AGENTGRID_ADAPTER=adapter-opencode` / `AGENTGRID_ADAPTERS=opencode` on
the node, and provide the model key (e.g. `GOOGLE_GENERATIVE_AI_API_KEY`). See
`docs/deploy/reverse-proxy.md` for TLS termination in front of the plain-HTTP
control plane.

**Custom adapter runtime images.** The base node image does NOT ship a coding
agent runtime — adapters are operator-provided on the host or layered into a
derivative image, matching the `permissionInterception`/sandbox threat model.
For `opencode` use the `OPENCODE_VERSION` build-arg above. For a custom
runtime (Claude Code, an internal agent, etc.) extend the base image with a
`Dockerfile` like:

    FROM ag-node:test
    # install your runtime / adapter binary, then:
    ENV AGENTGRID_ADAPTERS=your-adapter

and run it with the same `docker-compose` hardening (`read_only`,
`cap_drop: ALL`, `security_opt: no-new-privileges`, workspace/repo on a
writable volume). See `Dockerfile.node-daemon` for the build-time
`OPENCODE_VERSION` pattern to mimic.

**Unattended permission bypass (unsafe).** By default the `claude` and
`opencode` adapters run **safe**: they do NOT pass `--dangerously-skip-permissions`
/ `--auto`, so an unattended run blocks on the first interactive prompt rather
than auto-running destructive tools. To allow an unattended agent to proceed
without prompts you must opt in explicitly with
`AGENTGRID_UNSAFE_UNATTENDED=1` (or the per-adapter `AGENTGRID_OPENCODE_AUTO`
knob for opencode). The adapter prints a stderr warning when the bypass is on.
Agent must run in a sandbox before enabling the bypass. Wrapper adapters
(`adapter-claude` / `adapter-opencode`) drive the agent CLI as a subprocess:
their `permission_interception` capability is `wrapper`, **not** structured —
the bypass flag is the only knob they apply, so an unsandboxed wrapper adapter in
unsafe mode gives the agent full host access. Only structured-interception /
container backends count as isolation; the worktree is **not** a security
boundary.

## Build from source

Requires Rust (edition 2021), git, and a C toolchain. SQLite is bundled and TLS
is rustls-only — no system OpenSSL/SQLite. Linux only (x86_64 tier 1,
aarch64 tier 2).

    cargo build --release
    cargo test --workspace

Binaries: `agentgrid-control-plane`, `agentgrid-node-daemon`, `ag`,
`adapter-{mock,claude,opencode}`.

## Dev / ops notes

- Only one control-plane instance per SQLite DB: a second launch is refused via
  an exclusive flock. Never run two against the same data dir.
- The node daemon emits an `attempt started` event on spawn; a slow agent that
  is silent past the 30s assignment lease no longer triggers a duplicate attempt.
- A warning is logged when an adapter exits 0 but produces no events, surfacing
  silent agents that yield empty "succeeded" tasks.

### Trust & ownership model

The control plane is the trust root. A node is only trusted for the attempt it
was assigned: every `/v1/node/attempts/*` mutation checks authenticated node
**ownership** of the target attempt (a foreign attempt yields `403`/`404`) and
a per-attempt **fencing token** (`X-AgentGrid-Fencing-Token`); a stale token from
a superseded attempt/lease yields `409`. A revoked node fails auth at
`require_node_auth`. See [`docs/decisions/threat-model.md`](docs/decisions/threat-model.md).

### Event delivery semantics

Node → control-plane events are **idempotent** (`ON CONFLICT (attempt_id,
sequence) DO NOTHING`) and **durable**: the node persists each event to an on-disk
outbox before sending, so a kill `-9` or CP outage does not lose in-flight
events; on reconnect the un-acked tail is replayed. Event batches are bounded
(`AGENTGRID_MAX_EVENT_BATCH` count, `AGENTGRID_MAX_EVENT_BATCH_KB` bytes), and
events for a terminal attempt are rejected. A long-poll assignment never
double-delivers a task to two pollers (`WHERE status='queued'` CAS).

### Artifact retention, backup, upgrade & rollback

- **Artifacts:** per-attempt under `AGENTGRID_ARTIFACT_ROOT/<attempt_id>/<name>`;
  atomic upload (temp+rename) with server-side SHA-256 verification
  (client hash mismatch → 422). `cleanup_artifacts(<hours>)` drops the metadata
  row **and** the backing file, then removes now-empty attempt dirs. Upload size
  is capped (`AGENTGRID_MAX_ARTIFACT_MB`).
- **Backup:** `VACUUM INTO '<path>'` on the SQLite DB; back up the DB file plus
  the `AGENTGRID_ARTIFACT_ROOT` tree.
- **Upgrade:** forward-only migrations run on startup (`sqlx::migrate`); restart
  the control plane after replacing the binary. A node re-enrolls only if its
  persisted credential is gone.
- **Rollback:** keep the pre-upgrade DB snapshot; a downgraded binary fails loud
  against a DB with newer migrations. See
  [`docs/upgrade-0.1.0-to-0.1.1.md`](docs/upgrade-0.1.0-to-0.1.1.md).

See `docs/decisions/0001-mvp-scope.md` (ADR) and `docs/decisions/threat-model.md`.

### Compatibility matrix

| Component | Requirement | Status |
|-----------|-------------|--------|
| OS | Linux only — x86_64 tier 1 (Ubuntu 24.04, Debian 12/13), aarch64 tier 2. No kernel < 5.10, 32-bit, big-endian, or NFS workspaces. | enforced |
| SQLite | bundled `libsqlite3-sys` (3.40+); WAL, `synchronous=NORMAL`, `busy_timeout=5000`. No system SQLite lib. | bundled |
| TLS | `rustls` only — no system OpenSSL. | enforced |
| Runtime deps | none required — no Docker/Node/Python/Java/external DB at runtime (Node.js only for building the web UI). | documented |
| Rust | edition 2021, stable toolchain. | enforced |
| Transport (node channel) | long polling (WebSocket deferred). Public API under `/v1`. | as documented |
| Migrations | forward-only; downgrades fail loud against a newer-DB. | enforced |
| Release targets | `x86_64`/`aarch64`-`unknown-linux-musl` (+ `x86_64-gnu` fallback). | in `release.yml` |

OpenAPI is not yet auto-generated; the `/v1` HTTP surface is documented inline in
`crates/control-plane/src/lib.rs` route declarations and `docs/decisions/`. A
hand-maintained OpenAPI 3.0 summary of the public surface lives at
[`docs/openapi.yaml`](docs/openapi.yaml). Track `#21` (Typed API errors /
OpenAPI) for the generated document.

## License

MIT — see [LICENSE](LICENSE).
