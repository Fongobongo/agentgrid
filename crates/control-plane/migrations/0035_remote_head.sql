-- agentgrid control-plane schema (hardening P1 item 32)
-- Persist the remote (upstream) HEAD captured at attempt *start* and at
-- attempt *finish*, so audits / diffs / quarantine decisions can reconstruct
-- what upstream looked like when the attempt began and how it moved during
-- the attempt. Independent of resolved_base_sha (the prepared base the agent
-- built on). Populated by the node daemon; NULL when not a git repo or unset.
ALTER TABLE attempts ADD COLUMN remote_head_at_start TEXT;
ALTER TABLE attempts ADD COLUMN remote_head_at_finish TEXT;
