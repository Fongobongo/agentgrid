-- Stage 4.2: revoked user sessions (JWT jti blocklist).
CREATE TABLE IF NOT EXISTS revoked_sessions (
    jti           TEXT PRIMARY KEY,
    username      TEXT NOT NULL,
    revoked_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_revoked_sessions_username ON revoked_sessions (username);
