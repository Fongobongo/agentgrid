-- agentgrid control-plane schema (P2 item 32-5)
-- Persist the exact upstream commit each attempt started from, so audits /
-- diffs can reconstruct the prepared base even when the task row's base_commit
-- was unset (branch default). Currently populated by the node daemon for the
-- explicitly pinned base_commit path; default-branch checkouts are left NULL
-- until the node daemon resolves HEAD per attempt (P1 follow-up).
ALTER TABLE attempts ADD COLUMN resolved_base_sha TEXT;
