-- Node-loss retry. A lost attempt is the one failure the control plane knows
-- nothing about: the node died mid-run, so there is no completion report and
-- no evidence of what the agent already did. Until now that made the task
-- terminal (`failed` / `node_lost`), which kept the platform honest but left
-- the operator to re-queue by hand.
--
-- `retry_on_node_loss` = 1 (default) lets a node loss spend the task's retry
-- budget exactly like a reported failure. The budget is still the existing
-- `max_attempts`, so a default task (`max_attempts = 1`) never retries and
-- nothing changes for it; this column only decides whether a node loss is
-- *allowed* to spend that budget. Set it to 0 for tasks whose side effects
-- must not be repeated (pushes, external calls) and node loss stays terminal.
ALTER TABLE tasks ADD COLUMN retry_on_node_loss INTEGER NOT NULL DEFAULT 1;

-- `avoid_node_id`: the node that lost the last attempt. Written when an
-- attempt is lost, cleared when the next attempt is assigned. The scheduler
-- skips it while another node is online, so "retry on a new node" really means
-- a new node; when it is the only node online the skip is lifted rather than
-- parking the task in `queued` forever.
ALTER TABLE tasks ADD COLUMN avoid_node_id TEXT;
