-- Per-task cost ceilings (autopilot safety): the node kills the attempt with
-- error_code = "budget_exceeded" when reported adapter usage passes either
-- ceiling. NULL = unbounded (budgets still live on workflow templates).
ALTER TABLE tasks ADD COLUMN max_tokens INTEGER;
ALTER TABLE tasks ADD COLUMN max_cost_cents INTEGER;
