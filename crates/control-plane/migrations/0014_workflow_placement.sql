-- agentgrid control-plane schema (Stage 8): per-step node placement so a
-- workflow can spread roles across different nodes.
ALTER TABLE workflow_steps ADD COLUMN requested_node_id TEXT;
