-- agentgrid control-plane schema (P2 item 21)
-- Enforce uniqueness of (conversation_id, seq) so concurrent appends cannot
-- silently collide. This is the DB-side backstop for the atomic seq
-- allocation in `append_conversation_message`; the handler still retries on
-- the rare SQLITE_BUSY/UNIQUE collision, but a dropped invariant here would
-- let two messages share a sequence and break ordered reads.
CREATE UNIQUE INDEX IF NOT EXISTS ux_conv_msgs_seq
    ON conversation_messages (conversation_id, seq);
