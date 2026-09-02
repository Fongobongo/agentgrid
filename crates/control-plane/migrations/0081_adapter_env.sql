-- CP-managed adapter environment, pushed to nodes via PollResponse.adapter_env
-- and injected into attempt process env (e.g. ANTHROPIC_BASE_URL /
-- ANTHROPIC_AUTH_TOKEN to point claude-code at a non-Anthropic endpoint).
-- adapter='*' applies to every adapter; node_id NULL = global.
CREATE TABLE IF NOT EXISTS adapter_env (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    adapter TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    node_id TEXT NOT NULL DEFAULT '', -- '' = global
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    UNIQUE (adapter, key, node_id)
);
