-- Stage 13: MCP server registry. Operator-managed stdio servers a profile may
-- attach to a session. `env_requirements` lists env var *names* only (the node
-- resolves values from its own env at spawn; never stored here, like secret
-- requirements). `enabled` lets an operator disable a server without deleting
-- it so running sessions don't break.
CREATE TABLE mcp_servers (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    command           TEXT NOT NULL,
    args              TEXT NOT NULL DEFAULT '[]',        -- JSON array of strings
    env_requirements  TEXT NOT NULL DEFAULT '[]',       -- JSON array of names
    enabled           INTEGER NOT NULL DEFAULT 1,
    created_at        TEXT NOT NULL
);
