-- Control-plane HA cluster state (single row). Single mode never reads this
-- table; HA data nodes update it on promotion so every write transaction can
-- re-verify the epoch it was admitted under.
CREATE TABLE IF NOT EXISTS ha_cluster_state (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    cluster_id      TEXT    NOT NULL DEFAULT '',
    epoch           INTEGER NOT NULL DEFAULT 0,
    term            INTEGER NOT NULL DEFAULT 0,
    leader_id       TEXT,
    schema_version  INTEGER NOT NULL DEFAULT 0,
    repl_seq        INTEGER NOT NULL DEFAULT 0,
    updated_at      TEXT    NOT NULL DEFAULT ''
);

INSERT OR IGNORE INTO ha_cluster_state (id) VALUES (1);
