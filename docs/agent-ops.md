# Running coding agents on the grid (operations runbook)

How to prepare, enroll, and run real coding agents (opencode, Claude Code,
…) through agentgrid nodes — on one box or across several hosts. The adapter
*contract* lives in `adapters.md`; this doc is about *operating* the agents
themselves.

## Before launching: update the agent

Agents evolve fast and stale builds break in confusing ways. Update on
**every host that will run the agent**, right before enrolling the node:

```bash
# opencode — reinstall pulls the latest release into ~/.opencode/bin
curl -fsSL https://opencode.ai/install | bash
~/.opencode/bin/opencode --version

# claude code
npm i -g @anthropic-ai/claude-code && claude --version
```

The node daemon probes `adapter-<id> --version` at startup; if the agent
binary is missing from `$PATH` the node comes up **degraded** (visible in
`/v1/nodes` and the UI) instead of crashing. So: update the agent, then
(re)start the node daemon with the agent's bin dir on `PATH`.

## Auth / credentials

Agents authenticate independently of agentgrid. Two delivery channels:

1. **Interactive login once per host** — e.g. `opencode auth login` writes
   `~/.local/share/opencode/auth.json`; the adapter inherits it.
2. **Env injection** — start the node daemon with
   `AGENTGRID_ADAPTER_ENV="OPENCODEZEN_API_KEY=..."` (comma-separated
   `K=V` pairs). Only those keys are forwarded to the adapter child.

Never put secrets in prompts or commit them; `.env` stays gitignored.

## Unattended (headless) mode

Real agents refuse destructive/interactive prompts unless explicitly told
otherwise. agentgrid requires a double ack — set BOTH on the node daemon:

```bash
AGENTGRID_UNSAFE_UNATTENDED=1 AGENTGRID_I_UNDERSTAND_UNSAFE=1
```

With the ack present, `adapter-opencode` adds `--auto`; without it, the node
runs in safe mode and the UI flags it (`unsafe_active` badge). Prefer a
sandbox (Docker/landlock) for anything but throwaway prompts.

## Concurrency: one opencode per host

opencode keeps its own SQLite state DB; two concurrent runs on the same host
fail with `database is locked`. Set `AGENTGRID_MAX_CONCURRENCY=1` on any
node whose adapters include `opencode`. Scale by adding hosts, not slots.

## Enrolling a node (any host)

```bash
# control plane side (admin JWT):
curl -X POST $BASE/v1/nodes/enrollment-token -H "authorization: Bearer $JWT"
# node side — the token is single-use and persisted as credential.json:
AGENTGRID_SERVER=http://<cp-host>:7800 \
AGENTGRID_DATA_DIR=/var/lib/agentgrid \
AGENTGRID_NODE_NAME=box-1 \
AGENTGRID_WORKSPACE_ROOT=/var/lib/agentgrid/work \
AGENTGRID_REPOSITORY_ROOT=/var/lib/agentgrid/repos \
AGENTGRID_ADAPTERS="opencode" \
AGENTGRID_ENROLL_TOKEN=<token> \
agentgrid-node-daemon   # first run enrolls, then keep it running
```

Remote hosts: upload the `agentgrid-node-daemon` + `adapter-*` binaries
(`tests/e2e/remote-ssh.py --file …` works without sshpass), make sure the
remote can reach the CP's listen address, and launch detached
(`setsid nohup … </dev/null &`). Use `AGENTGRID_ALLOW_ROOT=1` when running
as root.

## Troubleshooting (learned the hard way)

| Symptom | Cause / fix |
|---|---|
| Node `degraded` after enroll | adapter binary not on node `$PATH`; upload/install it, restart daemon |
| Enroll fails with empty body | URL mismatch: daemon posts to `/v1/node/enroll` (singular). Upgrade the daemon binary |
| opencode `database is locked` | two opencode runs on one host; set `AGENTGRID_MAX_CONCURRENCY=1` |
| opencode `Unexpected error`, no detail | auth missing — run `opencode auth login` on that host or inject the key via `AGENTGRID_ADAPTER_ENV` |
| `pkill -f agentgrid-node-daemon` kills your own shell | the pattern matches the invoking `bash -c` cmdline; kill by PID or use `pkill -f 'daemo[n]'` |
| Remote daemon dies when SSH session closes | launch with `setsid nohup … </dev/null &` |

## Two-host smoke test

A minimal end-to-end check that both nodes actually work:

```bash
# submit the same trivial task twice; with concurrency=1 per node the
# scheduler places one on each host
curl -X POST $BASE/v1/tasks -H "authorization: Bearer $JWT" \
  -H 'content-type: application/json' \
  -d '{"prompt":"Create a file named greeting.txt containing exactly: hello from agentgrid. Do nothing else.","repository":"*","adapter":"opencode","timeout_secs":600}'
```

Both tasks should reach `succeeded`, each with its attempt on a different
node (check `starting attempt` lines in the node logs).
