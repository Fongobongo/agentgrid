-- Hardening P1 item 11: record which artifacts a node staged locally but
-- could not deliver before completion (control plane was down). JSON array of
-- artifact names; NULL/empty = nothing owed. The node retries these on the
-- next startup; operators can see the outstanding set via the attempts row.
ALTER TABLE attempts ADD COLUMN pending_artifacts TEXT;
