-- Competitor-gap feature (GitHub write-back): task-level metadata that makes
-- a successful attempt push its agent branch to the origin remote, open a PR,
-- and comment on a linked GitHub issue. Everything is best-effort on the node
-- (a failure emits a log event, never fails the task).
--
-- github_push drives the node behaviour; github_repo is the `owner/name`
-- full_name, github_issue the issue number, github_base_ref the PR base
-- (falls back to the repository's default_branch when NULL).
ALTER TABLE tasks ADD COLUMN github_push INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN github_repo TEXT;
ALTER TABLE tasks ADD COLUMN github_issue INTEGER;
ALTER TABLE tasks ADD COLUMN github_base_ref TEXT;
