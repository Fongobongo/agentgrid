-- Stage 9.2: scope an approval request (tool_call / session / step / command /
-- duration) so operators see what they are approving.
ALTER TABLE approvals ADD COLUMN scope TEXT NOT NULL DEFAULT 'session';
