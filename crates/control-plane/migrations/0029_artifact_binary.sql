-- Stage 2.2 binary-safe artifact API: record media type + content hash so
-- non-UTF-8 artifacts (binary patches, archives, images) round-trip without
-- the UTF-8 JSON body loss of the legacy text upload. Both columns are
-- nullable: the legacy text endpoint never set them.
ALTER TABLE artifacts ADD COLUMN media_type TEXT;
ALTER TABLE artifacts ADD COLUMN sha256     TEXT;
