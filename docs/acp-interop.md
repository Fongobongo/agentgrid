# ACP client interoperability (Poracode / Lightcode)

Stage 6 shipped `agentgrid-acp-agent`: a stdio ACP *agent* that bridges any
ACP-speaking client to the Agentgrid control plane. This note records what
works today and the known gaps, as a spike (no live Poracode/Lightcode runs
were available in this environment).

## What an ACP client gets for free

`agentgrid-acp-agent` speaks the standard ACP agent role over stdio, so any
compliant client can drive it without changes:

- `initialize` → returns protocol version `0.1`, empty capabilities.
- `session/new` → mints an Agentgrid session id, stores agent/model/cwd.
- `session/prompt` → creates an Agentgrid task, then streams task
  `status`/`tool_call`/`file_change`/`progress`/`result`/`error` events back as
  `session/update` until the task reaches a terminal state.
- `session/cancel` → cancels the backing task.
- `session/request_permission` → relayed from the control plane's approval
  queue; the client's `allow`/`deny` answer is posted back to the CP.

Environment: `AGENTGRID_SERVER` (required), `AGENTGRID_TOKEN` (optional).

## Known gaps / non-standard extensions

- **Extension methods are Agentgrid-specific.** Any `method` starting with `_`
  is routed to `handle_extension`; currently `_agentgrid/nodes`
  (`GET /v1/nodes`) and `_agentgrid/task_eligibility`
  (`GET /v1/tasks/{id}/eligibility`). Unknown `_` methods return a clean RPC
  error. A plain ACP client should ignore these.
- **No `session/load` / `session/resume` passthrough.** The gateway maps each
  `session/prompt` to a *new* Agentgrid task; it does not replay prior session
  history into the client. Multi-turn context stays inside the Agentgrid task's
  event log.
- **No `session/update` from client → control plane.** Client-sent
  `session/update` notifications are accepted (and ignored) today; they are not
  forwarded to the CP.
- **Capabilities negotiation is minimal.** `initialize` returns empty
  `capabilities`; clients that require specific capability flags may need a
  shim.

## Compatibility verdict

Poracode and Lightcode are ACP clients; both should connect to
`agentgrid-acp-agent` over stdio and run single-shot tasks. The gaps above are
ergonomic (resume, richer capability negotiation), not blocking. Verifying the
end-to-end handshake against live Poracode/Lightcode builds is left as a
follow-up (needs those binaries + credentials).
