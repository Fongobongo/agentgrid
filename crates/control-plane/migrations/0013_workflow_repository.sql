-- agentgrid control-plane schema (Stage 7.3): workflow runs target a repository
-- so their step tasks can be scheduled against enrolled nodes.
ALTER TABLE workflow_runs ADD COLUMN repository TEXT;
