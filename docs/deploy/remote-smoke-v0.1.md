## Live smoke run (v0.1.0-smoke) — remote host 191.96.11.161

Verified AgentGrid deploys end-to-end on a second Linux box (Debian 12, 2 vCPU,
4 GB RAM); no source checkout needed, just four release binaries.

### What ran

- Control-plane (SQLite WAL, JWT auth, /v1 API)
- Two node-daemon instances: fake-acp + mock adapters
- Same-machine ws transport (long poll fallback not exercised on this smoke)
- First-boot setup: /v1/auth/setup → JWT → /v1/nodes/enrollment-token → /v1/node/enroll
- Task submitted via /v1/tasks: **succeeded**; stdout/result events streamed and
  persisted

### Artifacts captured

- `bin/agentgrid-control-plane`
- `bin/agentgrid-node-daemon`
- `bin/ag`
- `bin/adapter-fake-acp`

Stored under /root/ag-smoke during the run; removed after. See
`tests/e2e/remote-ssh.py` for the exact SSH invocation contract.
