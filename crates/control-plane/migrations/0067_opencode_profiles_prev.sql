-- opencode profiles: keep the previous config next to the current one so an
-- operator can roll a node back without re-pasting JSON. Deferred from the
-- profiles introduction under "record profile revisions" — shrunk to "one
-- revision back" to keep the table shape unchanged (rollback writes the
-- current body into prev, drops the far older copy).
--
-- Rationale for only one revision: the dashboard never displays older than
-- "previous" (audit rows cover the *when*; config bodies cover the *what*
-- for at most one step of rollback). If a deeper audit trail becomes
-- necessary, an `opencode_profile_revisions` table can be added later via
-- a migration that re-splits the column into a child FK.
ALTER TABLE opencode_profiles ADD COLUMN prev_config_json TEXT;
ALTER TABLE opencode_profiles ADD COLUMN prev_hash TEXT;
