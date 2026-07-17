# agentgrid

Distributed orchestrator for coding agents. A control plane dispatches tasks to
node daemons, each running an LLM agent adapter (Claude Code / Codex / OpenCode)
in an isolated git worktree. SQLite WAL on local disk, single active
control-plane instance.

> Status: MVP 0.1 — see [CHANGELOG.md](CHANGELOG.md).

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

Control plane: http://127.0.0.1:7800 (default login `admin` / `changeme`). Two
node daemons (mock adapters) come online; submit a task:

    export AGENTGRID_SERVER=http://127.0.0.1:7800
    ag login admin changeme
    ag run <repo> "your prompt here" --adapter mock

Tear down: `./deploy/compose/down.sh` (or `docker compose down`).

## OpenCode node (optional)

The `opencode` CLI is operator-provided by default. To bake it into a portable
node image:

    docker build --build-arg OPENCODE_VERSION=1.17.16 -f Dockerfile.node-daemon -t ag-node-opencode .

Then set `AGENTGRID_ADAPTER=adapter-opencode` / `AGENTGRID_ADAPTERS=opencode` on
the node, and provide the model key (e.g. `GOOGLE_GENERATIVE_AI_API_KEY`). See
`docs/deploy/reverse-proxy.md` for TLS termination in front of the plain-HTTP
control plane.

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

See `docs/decisions/0001-mvp-scope.md` (ADR) and `docs/deploy/`.

## License

MIT — see [LICENSE](LICENSE).
