# ADR 0018: Stage 13 closers — Swarms import, context optimizer, Guild memory

Status: accepted (Stage 13)

## 1. Swarms plan import

Local dependency-aware skills (`SKILL.md` + `depends_on` metadata) import as
a `WorkflowTemplate`: one worker step per skill, edges from the CSV
`depends_on` key, DAG-validated before anything reaches the server
(`agentgrid_skills::swarms_to_template`; unknown/self deps fail closed,
cycles fail DAG validation).

```bash
# Print the DAG (validates locally, no server round-trip):
ag workflows import ./my-swarm --name my-swarm
# …and create it on the server:
ag workflows import ./my-swarm --name my-swarm --create
```

Skill bodies stay out of the template (progressive disclosure): the step
prompt names the skill; activation materialises the body on the node as
before. Trust still gates activation — import builds the DAG, it does not
trust the skills.

## 2. RTK/Headroom as the optional context optimizer

No new optimizer engine: the existing `agentgrid_common::compress`
(dup-line collapse + newline-safe truncation with a saved-bytes metric) is
the optimizer, applied to the injected context-pack body when
`AGENTGRID_CTX_BUDGET_BYTES` is set (default 0 = off). The `context_pack`
status event records `trimmed_bytes`; the truncation marker states the
dropped count; the full pack stays in the on-disk index cache under its
`cache_key` — raw evidence is never lost, only the prompt copy is budgeted.

## 3. Guild shared memory as an optional MCP profile

Guild is an external shared-memory MCP server. No code ships for it (external
binary, like Zeroshot/CTX): the operator registers it in the MCP registry
(operator-managed trust gate, same as skills) and scopes it to a profile:

```bash
# Register once (command runs per session over stdio; the ACP adapter spawns
# it from SessionNewParams.mcp — our side never executes it directly):
ag mcp create guild-memory "Guild shared memory" /usr/local/bin/guild-mcp \
  --arg serve --arg --socket /run/guild/sock \
  --env-requirement GUILD_API_KEY

# Attach it to one profile only (empty subset = all enabled servers):
ag profiles create guild-worker --autonomy l2 \
  --system-prompt "Use the guild memory tools for cross-session facts."
# …then set the profile's mcp_server_ids = ["guild-memory"] via the API
# (AgentProfileCreate carries it; the CLI create flags do not yet):
curl -s -X POST "$CP/v1/profiles/guild-worker" \
  -H "authorization: Bearer $JWT" -H 'content-type: application/json' \
  -d '{"system_prompt":"Use the guild memory tools for cross-session facts.",
       "autonomy":"l2","mcp_server_ids":["guild-memory"]}' \
| python3 -c 'import sys,json;print(json.load(sys.stdin)["revision"])'
# …and activate the printed revision:
# ag profiles activate guild-worker <rev>
```

Secrets stay names-only in the registry/profile; the node resolves values
from its own env at session spawn. Fail-closed as usual: MCP spawn failure
fails the session setup, never silently drops the memory tools.
