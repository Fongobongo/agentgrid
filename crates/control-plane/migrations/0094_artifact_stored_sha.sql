-- HA artifact replication: SHA-256 of the bytes actually stored on disk
-- (the .zst frame for compressed logs), so a peer can verify a pulled copy.
-- `artifacts.sha256` stays the uncompressed content hash. NULL for rows
-- written before this migration; those are not pullable on demand.
ALTER TABLE artifacts ADD COLUMN stored_sha256 TEXT;
