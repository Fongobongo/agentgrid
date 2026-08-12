-- opencode profile revision history. 0067 added a single prev slot; operators
-- then asked "roll back two steps" after two separate bad PUTs landed before
-- the symptom surfaced. Revisions are append-only — every PUT that overwrote
-- an existing row moves the old (config_json, hash) here, and rollback pops
-- from the head in steps. `prev_config_json`/`prev_hash` stay as the fast
-- path for "show the rollback target" on the profile card; this table adds
-- depth.
CREATE TABLE IF NOT EXISTS opencode_profile_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    profile_id TEXT NOT NULL REFERENCES opencode_profiles(id) ON DELETE CASCADE,
    config_json TEXT NOT NULL,
    hash TEXT NOT NULL,
    saved_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_opencode_revs_profile
    ON opencode_profile_revisions(profile_id, id DESC);
