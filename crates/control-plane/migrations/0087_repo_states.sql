-- Plan 2.5 (#204): per-repository attach state reported by the node
-- heartbeat (`cloning` | `ready` | `invalid` + error text), stored as a
-- JSON array of {name, state, error} — same shape as the existing
-- `adapters` / `repositories` columns. '[]' = unknown (legacy node).
ALTER TABLE nodes ADD COLUMN repo_states TEXT NOT NULL DEFAULT '[]';
