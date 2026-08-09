-- Plan 1.3 (#6): full-text search over tasks.
-- FTS5 virtual table over (prompt, repository, id); triggers keep it in sync
-- with the tasks table on insert/update/delete. Ranked by bm25 in queries.

CREATE VIRTUAL TABLE IF NOT EXISTS tasks_fts USING fts5(
    id UNINDEXED,
    repository,
    prompt,
    content = 'tasks',
    content_rowid = 'rowid'
);

-- rowid: tasks is a TEXT-PK table; rowid is the implicit integer key, which
-- FTS5 requires as the content_rowid.

CREATE TRIGGER IF NOT EXISTS tasks_fts_ai AFTER INSERT ON tasks BEGIN
    INSERT INTO tasks_fts (rowid, id, repository, prompt)
    VALUES (new.rowid, new.id, new.repository, new.prompt);
END;

CREATE TRIGGER IF NOT EXISTS tasks_fts_ad AFTER DELETE ON tasks BEGIN
    INSERT INTO tasks_fts (tasks_fts, rowid, id, repository, prompt)
    VALUES ('delete', old.rowid, old.id, old.repository, old.prompt);
END;

CREATE TRIGGER IF NOT EXISTS tasks_fts_au AFTER UPDATE ON tasks BEGIN
    INSERT INTO tasks_fts (tasks_fts, rowid, id, repository, prompt)
    VALUES ('delete', old.rowid, old.id, old.repository, old.prompt);
    INSERT INTO tasks_fts (rowid, id, repository, prompt)
    VALUES (new.rowid, new.id, new.repository, new.prompt);
END;
