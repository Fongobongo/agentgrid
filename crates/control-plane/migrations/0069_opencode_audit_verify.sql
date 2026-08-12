-- opencode audit: add the node-side `opencode debug config` oracle outcome.
-- Applied after the apply lands on disk; CP-side check (allow-list + shape)
-- catches typos but can't catch opencode-semantics issues like an unknown
-- MCP transport. The oracle runs at apply time and forwards the verdict so
-- the dashboard can flag "looks legal but the binary barfs" rows.
ALTER TABLE opencode_config_audit ADD COLUMN verify TEXT;
