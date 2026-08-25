-- Audit follow-up: the repeated-handoff circuit breaker rescanned every
-- workflow_messages row of a run on each 5s budget tick. Persist the streak
-- incrementally: emit_workflow_message updates (current streak, its
-- all-time max, and the last (from,to) pair) in the same transaction as the
-- insert, so the tick reads three columns. The breaker trips on the MAX
-- (a runaway ping-pong must stay tripped even after a healthy broadcast
-- resets the live streak). Startup reconcile replays history once for runs
-- created before this migration.
ALTER TABLE workflow_runs ADD COLUMN handoff_streak INTEGER NOT NULL DEFAULT 0;
ALTER TABLE workflow_runs ADD COLUMN handoff_streak_max INTEGER NOT NULL DEFAULT 0;
ALTER TABLE workflow_runs ADD COLUMN handoff_last_from TEXT;
ALTER TABLE workflow_runs ADD COLUMN handoff_last_to TEXT;
