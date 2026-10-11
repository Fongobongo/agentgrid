-- Project assembly: create the remote repo via the GitHub API when it does
-- not exist yet (node-side, best-effort). 0 = legacy behaviour (push only).
ALTER TABLE tasks ADD COLUMN github_create INTEGER NOT NULL DEFAULT 0;
