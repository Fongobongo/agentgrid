-- opencode-config management (feature "opencode profiles"): the control
-- plane is the source of truth for per-node opencode configuration.
--
-- A profile is a named bundle of opencode settings (model, small_model,
-- provider blocks, plugin npm refs, inline skill definitions). It is stored
-- as opaque JSON — the config schema belongs to opencode, not to us; the CP
-- validates syntax + a small key allowlist and lets the node-side
-- `opencode debug config` be the final oracle.
--
-- Why server-side storage: without it each node operator edits
-- ~/.config/opencode by hand; config drift between nodes is invisible from
-- the CP. With profiles the operator edits once, the change is pushed to
-- every node holding that profile via the existing WS control channel.
CREATE TABLE IF NOT EXISTS opencode_profiles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    -- Opaque opencode config (validated JSON object). No secrets: API keys
    -- stay in node-side env and are referenced as {env:VAR} placeholders.
    config_json TEXT NOT NULL,
    -- sha256 hex of config_json; compared by nodes to skip no-change writes.
    hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- A node applies at most one profile. NULL = node does not participate in
-- opencode profile sync (default; no behavioural change for existing nodes).
ALTER TABLE nodes ADD COLUMN opencode_profile_id TEXT
    REFERENCES opencode_profiles(id) ON DELETE SET NULL;

-- Audit: every application of a profile by a node lands here so the web UI
-- can answer "who is on which profile" and "when did node X last apply".
CREATE TABLE IF NOT EXISTS opencode_config_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    node_id TEXT NOT NULL,
    profile_id TEXT,
    hash TEXT NOT NULL,
    trigger TEXT NOT NULL -- 'ws_push' | 'error_threshold' | 'interval' | 'startup'
);
CREATE INDEX IF NOT EXISTS idx_opencode_audit_node
    ON opencode_config_audit(node_id, at DESC);

-- Per-task opencode overrides shipped to the adapter via the assignment
-- (OPENCODE_CONFIG_CONTENT on the node; dies with the process — never
-- persisted server-side beyond the config's own lifetime).
ALTER TABLE tasks ADD COLUMN opencode_override TEXT;
