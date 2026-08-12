# Opencode profiles (agentgrid)

Control-plane-hosted opencode configuration. Each profile is a named JSON document with a strict top-level allowlist; the CP keeps them under `opencode_profiles` (migration 0066), and a node binds exactly one at a time through `nodes.opencode_profile_id`.

## Flow

1. **Operator upserts** a profile: `PUT /v1/opencode-profiles/{name}` with the body `{ "config": { ... } }`. The CP strips keys outside the allowlist, computes sha256 over the serialised result, and returns the hash.
2. **CP multicasts** the change to every enrolled node on its existing WebSocket control channel as `NodeWsMsg::ConfigUpdate { profile_id, hash }`.
3. **Node pulls** its active config from `GET /v1/node/opencode-config/active` (node-auth, `Extension(AuthedNode)`), compares its sha256 against the on-disk `~/.config/opencode/opencode.json`, and applies atomically when the hash differs: tmp + fsync + rename, keeping the previous file next door as `.agentgrid.bak`.
4. **Audit**: every apply posts a row via `POST /v1/nodes/{id}/opencode-audit` naming the trigger (`ws_push` | `error_threshold` | `interval` | `startup`).
5. **Per-attempt override**: `CreateTaskRequest.opencode_override { model, small_model, config }` flows into `Assignment::opencode_override` and, when the assigned adapter is opencode-only, joins into the child env as `OPENCODE_CONFIG_CONTENT`. Process-bound, no file writes — the node's on-disk profile stays clean for the next attempt.

## CLI

```
ag opencode profile list                         # all profiles (id, name, hash)
ag opencode profile show <name>                  # one profile with full config (includes `prev` if a rollback
target exists)
ag opencode profile set  <name> --config <file>  # upsert from JSON file (or `-` for stdin)
ag opencode profile set  <name> --config <file> --expires-at 2026-01-01T00:00:00Z  # auto-expire
ag opencode profile delete <name>
ag opencode profile delete <name> --fallback <other>  # move assigned nodes onto <other> first
ag opencode profile rollback <name>              # swap back one revision
ag opencode profile assign <node-id> --profile <name>   # bind node → profile
ag opencode profile assign <node-id> --clear            # detach
ag opencode profile ab <name> --other <other> --percent N   # A/B split nodes on either arm
ag opencode node audit    <node-id>              # apply history
```

Every PUT stashes the pre-write body as `prev` so a one-step `rollback`
endpoint exists. The far-older copy is dropped — deeper history requires a
follow-up migration adding an `opencode_profile_revisions` table.

Per-attempt overrides on `ag run`:

```
ag run --adapter opencode --opencode-model anthropic/claude-sonnet-4.5 -- repository "…"
ag run --adapter opencode --opencode-small-model anthropic/claude-haiku -- repository "…"
```

## Web

`/#/opencode`: list of cards (name, hash, updated_at, collapsible config preview), upsert textarea, assign-to-node dropdown, delete-with-confirm. Auto-polls every 5 s so fresh hash updates appear as nodes apply them.

## Self-healing

The node daemon counts consecutive config-class errors (invalid model, 401/403 on the provider, `model_not_found`, missing API key) on its stderr stream. When the streak crosses `AGENTGRID_CONFIG_PULL_AFTER_ERRORS` (default 3), the node pulls its active profile itself — recovering from a rolled-out model deprecation or a revoked credential without operator intervention. A successful completion resets the streak.

### Interval pull (off by default)

For paranoid deploys, `AGENTGRID_CONFIG_PULL_INTERVAL_SECS=<seconds>` turns on a dumb interval poll. The daemon pulls its active profile every N seconds and applies iff the hash drifted. **Default is off** — the WS push channel is healthy in practice and hash-drift convergence from heartbeats already covers the rare missed push. Use only when a *very* aggressive proxy in front of the CP makes websocket pushes unreliable. Enforced guard-rail: values below 30 s are ignored (tick frequency below that is just spam).

## TTL (auto-expire)

A profile can carry an absolute expiry (`expires_at`, RFC3339 UTC). When the
janitor ticks past it the profile is deleted exactly like a manual DELETE
(nodes are re-pointed off via `ON DELETE SET NULL` and woken with a
ConfigUpdate clear push; their last-applied on-disk config stays).

- Set: `ag opencode profile set <name> --config file.json \
  --expires-at 2026-01-01T00:00:00Z`, or the web upsert form's
  "expires at" field. Absent/empty = never expires; a PUT without
  `expires_at` clears a previous TTL.
- Sweep cadence: 15 s, same maintenance loop that reverts leases.
- `expires_at` is validated as RFC3339 on upsert — a typo fails loudly
  instead of silently never expiring.

## A/B percent assign

`POST /v1/opencode-profiles/{name}/assign-percent` (body `{ other, percent }`)
redistributes the nodes currently on either arm — `{name}` and `other` — so
that `percent`% land on `{name}` and the rest on `other`. Deterministic
(ordered by node id), so re-running with the same percent is stable; only
nodes already on one of the two arms move, the rest of the fleet is left
alone. Each moved node gets a ConfigUpdate push with its arm's hash. CLI:
`ag opencode profile ab <name> --other <other> --percent N`.

## Allowlist

Only these top-level keys are forwarded to the daemon:

```
model, small_model, provider, plugin, mcp, instructions, tools,
autoshare, share, snapshot, steps, temperature, top_p, top_k,
reasoning_effort, format, mode, provider_override
```

Anything else is stripped server-side before hashing, so a typo adds no entropy and no surprise device-specific settings never leak into managed nodes.

## Notes

- Idempotent: re-upserting the same normalised body is a no-op (same hash, no push to busy nodes, no audit noise).
- Revisions: every PUT stashes the pre-PUT body under `prev_config_json` plus
  the revision-history table; `POST /v1/opencode-profiles/{name}/rollback`
  walks back N steps (CLI: `ag opencode profile rollback <name> --steps=N`,
  web shows the previous body under each card so the operator can preview
  before swapping).
- Dry-run: `PUT /v1/opencode-profiles/{name}?dry_run=true` returns the
  post-sanitisation body + the hash that WOULD have been computed + the
  stripped-out unknown keys — without writing. The web "Preview" button
  drives this; the CLI's `ag opencode profile set` puts the preview on
  stderr and waits for an interactive confirm when stdin is a TTY.
- Shape contract: allowed keys are type-checked server-side (model a string,
  snapshot a bool, provider an object, plugin an array, etcetera). A wrong
  shape on an allowed key fails the PUT loudly instead of silently
  shipping the broken profile to every node.
- Drift detector: daemons report their applied on-disk hash on every
  heartbeat; the CP compares with the assigned profile's hash, writes an
  `opencode.drift` audit row, AND pushes a ConfigUpdate over the ws channel
  so the next tick converges the on-disk file. Self-healing within one
  heartbeat — a per-node UI "drift" badge was deliberately dropped because
  the auto-heal makes it nearly always transient.
- Multi-node: an assignment is per-node; a fleet without profiles continues as before (nodes start with no override and opencode behaves per its own fallback chain).
- Revision history: `opencode_profile_revisions` keeps every pre-PUT body; a walk-back of N steps is supported via `?steps=N`. The profile row's `prev_*` columns are the fast path for the most recent rollback target.
- Backup: `.agentgrid.bak` sits next to `opencode.json`; manual `mv` restores the previous profile instantly if the new one misbehaves.
