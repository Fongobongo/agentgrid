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
sandbox for anything but throwaway prompts: `AGENTGRID_SANDBOX=docker` (or
`systemd` for a cgroup scope, or `bwrap` on a host with no runtime and no root
— ADR 0022).

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

## Skills: importing a pack

The node discovers `SKILL.md` files under `<worktree>/.agents/skills` (project
tier), `~/.agents/skills` (user tier) and its managed root, then keeps only the
ones the control plane trusts (`ag skills list`). To install a pack — a repo
checkout, a published collection — into the user tier:

```bash
ag skills import ./some-skill-pack            # -> ~/.agents/skills/<name>/SKILL.md
ag skills import ./pack --dest /srv/skills    # explicit root
ag skills import ./pack --force               # install despite a Critical finding
```

The scanner gates the import (a skill with a `Critical` finding is refused, and
the findings are printed), the skill name is restricted to `[A-Za-z0-9._-]` so
the frontmatter cannot steer the write outside the destination, and re-running
is idempotent (`unchanged` / `replaced`). Exits 1 when anything was rejected.
Importing is onboarding, not a trust decision: the skill still needs
`ag skills trust <name> --source user` before an attempt will load it.

## Measuring an agent change (`ag eval`)

`crates/node-daemon/src/evals.rs` validates an attempt against pinned commands;
it does not tell you whether a change made the agent *better*. `ag eval` does:
a fixture of cases runs as ordinary tasks and comes back as a score.

```json
{
  "adapter": "opencode",
  "cases": [
    { "name": "rename-helper", "repository": "demo",
      "prompt": "Rename parse_cidr to parse_network in net.rs",
      "base_commit": "abc1234", "validation_command": "cargo test",
      "expect": "pass" },
    { "name": "known-limitation", "repository": "demo",
      "prompt": "…", "expect": "fail" }
  ]
}
```

```bash
ag eval fixture.json                      # PASS/FAIL per case + totals
ag eval fixture.json --json > base.json    # also a valid --baseline input
ag eval fixture.json --baseline base.json  # exit 1 on a regression
```

Three things are deliberate:

- **A case *is* a task.** It goes through `POST /v1/tasks`, so retries,
  budgets, validation feedback and the scheduler apply unchanged — a score
  cannot be better than production because it took a shortcut.
- **`adapter` is required** (fixture level, overridable per case). Defaulting
  it — to `mock`, say — would report a pass rate for something that never ran.
- **Tokens and cost are the budget's own numbers**: `metric` events carry
  per-event increments, so an eval cannot disagree with the bill.

`--baseline` reports a lost pass, a case that disappeared from the run, and a
cost increase above 25 % (below that is run-to-run noise). Each is a non-zero
exit; the JSON document also carries a `diffs` array. `expect: "fail"` pins a
known limitation so a fixture records it instead of quietly dropping it.
Fixture JSON is the input format — nothing in a fixture can execute code on the
control plane.

## Where is this defined? (`ag index --find`)

`ag index` parses a repository with tree-sitter (rust, typescript/tsx,
javascript, python, go, java, c, cpp) and reports every definition with the
type or module that encloses it:

```bash
ag index . --find parse_cidr
# crates/node-daemon/src/sandbox.rs:720  function  parse_cidr

ag index . --find SandboxKind::from_env
# crates/node-daemon/src/sandbox.rs:58   method    SandboxKind::from_env

ag index . --find bwrap --limit 5          # prefix matches
ag index . --json > symbols.json           # the whole packet
```

Matching is ranked — exact name, then prefix, then substring — so an exact hit
is never buried under near-misses, and a miss exits 1 (grep semantics, so it
can gate a script). Methods report their container, which is what disambiguates
two `new`s. Definitions only: an index of every identifier is a grep with extra
steps, and the question is where to *look*.

Two knobs matter in production:

- **`AGENTGRID_REPO_INDEX=1`** splices a compact digest of the packet above the
  profile text of every attempt (20 files, 12 names each, `(+N more)` for the
  rest — the full packet is at `<worktree>/.idx.json`). Off by default: it costs
  tokens on every attempt, so opt in when the adapter has no codebase awareness
  of its own.
- **`AGENTGRID_INDEX_BIN`** is the binary to run (default `ag`); it must be on
  the node's `PATH`. Any failure — missing binary, bad packet — leaves the
  profile text untouched, so a broken indexer degrades the prompt, never the
  attempt.

Cost is one parse per file, paid once per attempt (~1 s for a 200-file repo
built with `opt-level = "s"`). If the node runs many short attempts on a large
repo, leave the digest off and let the agent search.

## Why did this line change? (`AGENTGRID_NOTES`)

The full transcript of an attempt lives in the control-plane DB, which is the
right place for the event stream and the wrong place to look while reading a
diff. `AGENTGRID_NOTES` attaches a compact summary to the commit the attempt
produced, as a git note:

```bash
AGENTGRID_NOTES=local ag-node-daemon    # notes in the node's repo mirror
AGENTGRID_NOTES=push  ag-node-daemon    # …and push them with github_push tasks
```

```
agentgrid attempt 7626e5b6 (task df0c9d39)
commit: 7031ad67de608ef9a31d408eca03d458caf0b9d4
adapter: mock · status: succeeded · exit 0
repository: demo

prompt:
  write:hello.txt:hi

full transcript: ag events df0c9d39-… (control plane http://127.0.0.1:7824)
```

Notes go to `refs/notes/agentgrid`, never `refs/notes/commits`, so your own
notes are untouched. Read them with
`git notes --ref=agentgrid show <sha>` or `git log --notes=agentgrid`.

Five things worth knowing before turning it on:

- **The note contains the prompt.** A prompt is whatever an operator typed and
  can include a customer name or a pasted token. That is why the default is
  `off`, why `local` never leaves the host, and why `push` is gated twice: it
  needs `push` *mode* and a task that already has `github_push`. A typo in the
  variable is read as `off` — a typo must not start publishing prompts.
- **A note does not keep its commit reachable.** The note is keyed by the sha,
  and a note path is a tree entry, not a reference, so `git gc` may prune an
  unpushed commit while the note survives. The sha is repeated inside the note
  for exactly that case: the note stays readable when the object it names is
  gone.
- **The node protects its notes from its own fetches.** The repository mirror is
  a `--mirror` clone, so its fetch refspec is `+refs/*:refs/*` and the fetch
  runs `--prune` — a `local` note exists only on that node, so the next attempt
  on the same repository used to delete it (verified: one note before
  `fetch --prune`, zero after). Writing a note now adds
  `^refs/notes/agentgrid` to `remote.origin.fetch`, which carves the ref out of
  the fetch and protects it from the prune. `tests/e2e/run-notes.sh` pins this
  by running a second attempt after the first.
- **A failed note never fails an attempt.** Every outcome is a `session_note` /
  `session_note_failed` log event. A missing note is a smaller problem than a
  lost run.
- **Pushing is best-effort.** A non-fast-forward is the *expected* result when
  another node pushed a note for the same commit first; the note stays on this
  node and the push failure is a log line, not an error.

## Troubleshooting (learned the hard way)

| Symptom | Cause / fix |
|---|---|
| Node `degraded` after enroll | adapter binary not on node `$PATH`; upload/install it, restart daemon |
| Enroll fails with empty body | URL mismatch: daemon posts to `/v1/node/enroll` (singular). Upgrade the daemon binary |
| opencode `database is locked` | two opencode runs on one host; set `AGENTGRID_MAX_CONCURRENCY=1` |
| opencode `Unexpected error`, no detail | auth missing — run `opencode auth login` on that host or inject the key via `AGENTGRID_ADAPTER_ENV` |
| `pkill -f agentgrid-node-daemon` kills your own shell | the pattern matches the invoking `bash -c` cmdline; kill by PID or use `pkill -f 'daemo[n]'` |
| Remote daemon dies when SSH session closes | launch with `setsid nohup … </dev/null &` |
| Mirror clone fails: `destination path '<repo>' already exists` | the source git repo lives inside the node's repository root, so `git clone --mirror` collides with it; keep source repos outside `REPOSITORY_ROOT` |
| Adapter runs without `AGENTGRID_UNSAFE_UNATTENDED` despite setting it | on git tasks with `AGENTGRID_SANDBOX=none` the daemon strips the unsafe flag; opt back in with `AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX=1` |
| Fresh workflow run stays `pending` for minutes | the background ticker only resumes runs already `running`; a new run needs an explicit first `POST /v1/workflow-runs/{id}/tick` (run-workflow.sh does this in a loop) |

## Adapter env from the control plane

`ag adapter-env set <adapter|*> KEY VALUE` pushes builder/provider env to all
nodes (or `--node <id>` to one). Nodes receive the set on each poll and
inject it into attempt process env; node-local `AGENTGRID_ADAPTER_ENV` wins
on key collisions.

Point claude at a custom gateway endpoint:

    ag adapter-env set claude ANTHROPIC_BASE_URL https://agentrouter.org
    ag adapter-env set claude ANTHROPIC_AUTH_TOKEN sk-...
    ag adapter-env set claude ANTHROPIC_MODEL claude-opus-5

opencode custom-provider keys ride the same channel; the provider JSON
itself belongs in a CP opencode profile (`ag opencode put`), which nodes
sync automatically:

    ag adapter-env set opencode AR_KEY sk-...

## Egress proxy pool (CP-managed, since 0.4.3)

Route node traffic through a pool of proxies with automatic failover:

```bash
# on the control plane
ag proxy add http://user:pass@p1.corp:8080          # global pool
ag proxy add socks5://p2.corp:1080 --node node-b    # node-scoped
ag proxy ls                                         # id, url, node
ag proxy rm 3
```

Every node poll returns the effective list (global first, then node-scoped)
and the daemon routes its CP traffic, GitHub write-back API calls and
attempt environments (`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`, also inside
sandboxed containers) through the first alive URL. A connect/timeout marks
the proxy dead for 5 minutes and rotates to the next one; with the whole
pool dead the node falls back to direct egress (fail-open by design — a
dead proxy pool must not stop the fleet).

Override per node: `AGENTGRID_PROXY_URLS=url1,url2` in the daemon env
completely replaces the CP-pushed list (use when that node's network
differs from the fleet default). Caveat: the WS transport itself is not
proxied (no CONNECT support) — proxied nodes should run
`AGENTGRID_TRANSPORT=poll` or `auto` (auto falls back to poll).

A proxy is the right tool when the node needs egress *somewhere*. When it has
**no route to the control plane at all** but does reach a sibling node, that
sibling can carry its traffic instead — `AGENTGRID_RELAY_LISTEN` on the
reachable node, `AGENTGRID_SERVER=http://that-node:7900` on the stranded one.
Unlike the proxy pool this carries the WebSocket transport too, and it needs
no cluster token. See "Связь через промежуточный хост" in
`docs/runbook-transport.md` and ADR 0026.

Both mechanisms produce the same misleading answer: a dead proxy in front of the
control plane and a relay that lost its own upstream both reply `502`, which is
not a transport error, so nothing marks them dead. The node now leaves such an
*endpoint* after three consecutive `502`/`504` — `AGENTGRID_SERVERS` gives it
somewhere to go. That does not mark the *proxy* dead: the pool rotates on
connect/timeout only, so a proxy answering `502` stays in the pool and a node
with a single endpoint has nowhere to move.

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
