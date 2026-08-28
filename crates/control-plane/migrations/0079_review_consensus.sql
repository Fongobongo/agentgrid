-- Competitor-gap feature (consensus patch review, nitpicker-inspired):
-- consensus groups can be 'solve' (default: N models solve one task, patches
-- compared by SHA) or 'review' (N models review one diff, verdicts compared).
-- review groups point at the task whose changes.patch they review.
ALTER TABLE tasks ADD COLUMN consensus_mode TEXT NOT NULL DEFAULT 'solve';
ALTER TABLE tasks ADD COLUMN review_of TEXT;
