-- Plan 6.7 (zstd log compression): the backing file for a compressible
-- artifact is stored as <name>.zst on disk; the row keeps the UNCOMPRESSED
-- size (size_bytes stays the logical length every API already reports) and
-- this flag tells the read path which on-disk shape to decode. 0 = stored
-- plain (legacy rows / incompressible content).
ALTER TABLE artifacts ADD COLUMN stored_compressed INTEGER NOT NULL DEFAULT 0;
