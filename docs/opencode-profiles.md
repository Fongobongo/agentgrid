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
ag opencode profile show <name>                  # one profile with full config
ag opencode profile set  <name> --config <file>  # upsert from JSON file (or `-` for stdin)
ag opencode profile delete <name>
ag opencode profile assign <node-id> --profile <name>   # bind node → profile
ag opencode profile assign <node-id> --clear            # detach
ag opencode node audit    <node-id>              # apply history
```

Per-attempt overrides on `ag run`:

```
ag run --adapter opencode --opencode-model anthropic/claude-sonnet-4.5 -- repository "…"
ag run --adapter opencode --opencode-small-model anthropic/claude-haiku -- repository "…"
```

## Web

`/#/opencode`: list of cards (name, hash, updated_at, collapsible config preview), upsert textarea, assign-to-node dropdown, delete-with-confirm. Auto-polls every 5 s so fresh hash updates appear as nodes apply them.

## Self-healing

The node daemon counts consecutive config-class errors (invalid model, 401/403 on the provider, `model_not_found`, missing API key) on its stderr stream. When the streak crosses `AGENTGRID_CONFIG_PULL_AFTER_ERRORS` (default 3), the node pulls its active profile itself — recovering from a rolled-out model deprecation or a revoked credential without operator intervention. A successful completion resets the streak.

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
- Multi-node: an assignment is per-node; a fleet without profiles continues as before (nodes start with no override and opencode behaves per its own fallback chain).
- YAGNI: revisioned blobs are not kept — the profile row is the current snapshot and audit trails cover what ran when. Add history later if needed (YAGNI).
- Backup: `.agentgrid.bak` sits next to `opencode.json`; manual `mv` restores the previous profile instantly if the new one misbehaves.
