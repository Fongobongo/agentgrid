-- CP-managed egress proxy list, pushed to nodes via PollResponse.proxy_urls.
-- node_id NULL = global pool; a row with node_id applies only to that node
-- (node-specific entries are appended after global ones so global stays the
-- fallback when a node's own proxy dies).
CREATE TABLE IF NOT EXISTS proxies (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    url TEXT NOT NULL UNIQUE,
    node_id TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
