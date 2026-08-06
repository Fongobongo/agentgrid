-- Hardening P2 item 35: repository cache and workspace quota metrics.
-- Add repo_cache_bytes and workspace_bytes columns to nodes table.

ALTER TABLE nodes ADD COLUMN repo_cache_bytes INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN workspace_bytes INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_nodes_repo_cache_bytes ON nodes (repo_cache_bytes);
CREATE INDEX IF NOT EXISTS idx_nodes_workspace_bytes ON nodes (workspace_bytes);