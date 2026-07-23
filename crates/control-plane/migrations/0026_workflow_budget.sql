-- Stage 13 Loop Engineering: optional budget + circuit breaker attached to a
-- workflow template. Stored as JSON (WorkflowBudget serde). NULL means
-- "unbounded" (the historical default).
ALTER TABLE workflow_templates ADD COLUMN budget_json TEXT;
