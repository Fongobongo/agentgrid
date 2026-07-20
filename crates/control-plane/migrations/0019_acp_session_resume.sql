-- Stage 11.5: ACP session resume. `attempts.acp_session_id` stores the
-- session id the node received from `session/new`; `tasks.parent_acp_session_id`
-- is the session the node should resume (passed to ACP `session/new`).
ALTER TABLE attempts ADD COLUMN acp_session_id TEXT;
ALTER TABLE tasks ADD COLUMN parent_acp_session_id TEXT;
