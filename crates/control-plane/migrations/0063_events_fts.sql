-- Plan 2.10 (#21): contentless events FTS5 — every payload is baked into
-- the index at insert time and never joined back to task_events. The
-- triggers below push (id, attempt_id, payload) straight into the FTS
-- table; the BM25 query is the only consumer of indexed tokens.
CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
    attempt_id UNINDEXED,
    payload_text,
    tokenize = 'porter unicode61'
);

CREATE TRIGGER IF NOT EXISTS events_fts_ai AFTER INSERT ON task_events BEGIN
    INSERT INTO events_fts (rowid, attempt_id, payload_text)
    VALUES (NEW.ingest_id, NEW.attempt_id, NEW.payload);
END;
CREATE TRIGGER IF NOT EXISTS events_fts_bd AFTER DELETE ON task_events BEGIN
    DELETE FROM events_fts WHERE rowid = OLD.ingest_id;
END;
CREATE TRIGGER IF NOT EXISTS events_fts_bu AFTER UPDATE ON task_events BEGIN
    DELETE FROM events_fts WHERE rowid = OLD.ingest_id;
    INSERT INTO events_fts (rowid, attempt_id, payload_text)
    VALUES (NEW.ingest_id, NEW.attempt_id, NEW.payload);
END;
