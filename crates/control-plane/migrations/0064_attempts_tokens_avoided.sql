-- Plan 2.10 (#21): tokens_avoided_bytes metric on attempts — set by the
-- context ejector when it persists the resume digest (delta between the
-- previous attempt's full event-bytes and the digest size).
ALTER TABLE attempts ADD COLUMN tokens_avoided_bytes INTEGER;
