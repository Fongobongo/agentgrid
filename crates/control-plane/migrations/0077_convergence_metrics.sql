-- Competitor-gap feature (convergence metrics, loop-engineering):
-- * attempts.validation_rounds — how many feedback-loop rounds the node ran
--   (agent exit 0 but validation failed -> re-spawn with the error) before
--   the attempt converged. 0 = single-shot / ACP path.
-- * tasks.rework_of — the attempt id this task was reworked from
--   (POST /v1/attempts/{id}/rework). NULL for original tasks; walking the
--   chain gives the rework-iteration depth of a review loop.
ALTER TABLE attempts ADD COLUMN validation_rounds INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN rework_of TEXT;
