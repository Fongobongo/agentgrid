-- Stage 13 follow-up (per-profile MCP subset): an optional allow-list of
-- MCP server ids this profile attaches to its sessions. Empty (NULL/'[]') =
-- use every enabled server in the registry. Lets an operator restrict MCP
-- tools per adapter without splitting the registry.
ALTER TABLE agent_profiles ADD COLUMN mcp_server_ids TEXT NOT NULL DEFAULT '[]';
