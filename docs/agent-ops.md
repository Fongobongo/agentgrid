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

### Keeping the key out of the sandbox

Channel 2 hands the value to the agent, which means `docker inspect` and the
agent's own `/proc/self/environ` both show a live key. If that matters, write
the entry as a **route** instead of a value:

```bash
AGENTGRID_ADAPTER_ENV="ANTHROPIC_API_KEY@gateway=anthropic"
```

The value is the name of a route on a credential gateway, not a secret. At
attempt start the node mints an opaque **capability** for that route and puts
*that* in the agent's environment; the gateway swaps in the real key on its own
outbound leg. The node never holds the key, so there is nothing on it to leak.

The other half is the base URL — without it the capability is never presented
and the CLI talks to the provider directly, gets a 401, and nothing says why:

```bash
AGENTGRID_ADAPTER_ENV="ANTHROPIC_API_KEY@gateway=anthropic,\
ANTHROPIC_BASE_URL=http://egress-proxy:7845/p/anthropic"
```

`/p/<route>` is the gateway's proxy prefix. The capability arrives in the header
the provider expects (`x-api-key` for Anthropic, `Authorization` for an
OpenAI-shaped API) because the CLI is already configured with it — the SDK reads
`ANTHROPIC_API_KEY` and sends it as `x-api-key`, which is exactly what the
gateway validates. `Authorization: Bearer <capability>` is accepted as a
fallback for clients that only speak that header.

```bash
# the sidecar (see deploy/egress-proxy/)
AGENTGRID_CRED_GATEWAY_ADMIN=$(openssl rand -hex 32) \
AGENTGRID_CRED_GATEWAY_ROUTES="anthropic|x-api-key|https://api.anthropic.com" \
AGENTGRID_CRED_GATEWAY_KEYS="anthropic=sk-ant-..." \
  agentgrid-cred-gateway
```

and on the node daemon:

```bash
AGENTGRID_CRED_GATEWAY_URL=http://egress-proxy:7845
AGENTGRID_CRED_GATEWAY_ADMIN=<same token>
AGENTGRID_CRED_GATEWAY_TTL_SECS=21600   # optional, 6 h default
AGENTGRID_CRED_GATEWAY_QUOTA=200        # optional, requests per attempt
```

Capabilities are minted once per attempt (every feedback round reuses one),
revoked when the attempt ends, and expire on their own if the daemon dies.

It is **refused, not downgraded** — the attempt fails with
`infrastructure_failed` and the log names the fix — when:

- `AGENTGRID_CRED_GATEWAY_URL`/`_ADMIN` are unset (the attempt would have no
  credential at all);
- `AGENTGRID_SANDBOX` is not `docker` (without a separate PID namespace the
  agent can read the daemon's own environment, so a capability buys nothing);
- `network_mode=restricted` does not resolve to an egress network (the sandbox
  must reach the gateway and nothing else).

What it does **not** do: the agent still acts with a credential it holds while
the attempt runs — the permitted provider channel is an exfiltration path no
allowlist can tell from work (T13b). That is bounded by the quota, the TTL and
route binding, not prevented. And an entry still written as `NAME=value` is
still a key in the sandbox; only the marker changes anything.

To see which is which without reading the unit file: the daemon logs the split
at startup — a `WARN` naming every plain entry when nothing is gateway-delivered
(that is T13a, unfixed), and an `INFO` with `NAME->route` beside the plain names
when something is. Names and routes are not secrets; values are never logged.

The **Nodes** panel is the queryable version of the same fact: a node with any
plain entry carries a warning badge with the count, a fully migrated one a
neutral `N gateway` badge, one whose entries are exchanged a `N exchanged`
badge, and a node that has never reported carries nothing — the last case is not
"clean", it is "unknown" (an older daemon, or one that has not beaten since
upgrading). The counts travel in the heartbeat; the control plane never learns
the names or the values.

### Secrets that cannot be proxied: `@exchange`

`@gateway` works because the sandbox's traffic goes *through* the gateway. A
`git push` cannot: git speaks to GitHub itself, and rewriting its credential
would mean terminating its TLS. For those secrets, write the entry as an
exchange:

```bash
AGENTGRID_ADAPTER_ENV="GITHUB_TOKEN@exchange=github"
```

The gateway holds a GitHub App private key and, at attempt start, signs a
short-lived assertion with it and swaps that assertion for an **installation
token** — one hour, scoped to the installation's repositories. The sandbox gets
that real token and talks to GitHub directly.

```bash
# on the gateway
AGENTGRID_CRED_GATEWAY_EXCHANGE="github=github-app;app_id=123456;installation_id=78910;key=/etc/agentgrid/app.pem"
```

`api=` is optional and defaults to `https://api.github.com` (set it for GitHub
Enterprise). `,` separates routes, `;` separates fields. The key is read once at
startup, so a missing or non-PEM file refuses to start rather than failing an
attempt an hour later.

What this buys over the personal access token it replaces: the token expires on
its own, it is scoped to one installation, and it is revoked when the attempt
ends. What it does not buy: it is **real**, so it works from anywhere until it
expires — which is why exchange is refused unless the sandbox is on the enforced
egress network. An injected capability is worthless outside the gateway and
survives advisory networking; an exchanged token does not.

The provider decides the lifetime, not us. If the provider's window is shorter
than `AGENTGRID_CRED_GATEWAY_TTL_SECS`, the daemon logs a warning at mint — an
attempt that outlives the token starts failing mid-run with the provider's own
401, and that is worth knowing before it happens rather than after.

### TLS to the gateway (`AGENTGRID_CRED_GATEWAY_CA`)

Some CLIs refuse an `http://` base URL. Serving the gateway over TLS is for that
compatibility only — the agent's leg is inside the sandbox either way, so
terminating TLS here hides nothing from the agent. It is not a security upgrade;
do not reach for it as one.

```bash
# on the gateway: both or neither, never one
AGENTGRID_CRED_GATEWAY_TLS_CERT=/etc/agentgrid/gw.pem
AGENTGRID_CRED_GATEWAY_TLS_KEY=/etc/agentgrid/gw-key.pem
# on the daemon: the CA that signed it, for both ends of the connection
AGENTGRID_CRED_GATEWAY_CA=/etc/agentgrid/gw-ca.pem
```

Setting only one of `_TLS_CERT`/`_TLS_KEY` is a startup error, not a quiet
fallback to plaintext: a gateway that came up unencrypted because one variable
name was mistyped would still work, so nobody would notice.

The daemon does two things with the CA. It trusts it itself, because the mint
call is also HTTPS; and it bind-mounts it read-only into the sandbox at
`/etc/agentgrid/gateway-ca.pem`, pointing every trust-store variable the CLIs in
our images consult at that path (`SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`,
`CURL_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS`, `GIT_SSL_CAINFO`). Miss one and that
client fails with an unknown-issuer error that names nothing useful. The mount
happens only for `restricted` attempts, since an unrestricted one has no gateway
to talk to.

A configured CA that is not a readable PEM fails the attempt up front. This
matters more than it looks: docker turns a missing bind source into an empty
*directory* at the mount point, so the alternative is a directory where a
certificate belongs and a handshake error that points at the wrong thing.

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

`AGENTGRID_SANDBOX` is validated at startup: a value that is not
`none`/`docker`/`podman`/`systemd`/`bwrap` is a **startup error**, and with
`AGENTGRID_ENV=production` the daemon refuses to start with no sandbox at all.
A typo used to mean "run the agent on the host", which is the one thing this
variable exists to prevent. `docker` runs the attempt as the image's default
user — root in most images — with the worktree mounted read-write; set
`AGENTGRID_RUN_AS_USER=<uid[:gid]>` to give the agent its own identity (see
the file header of `crates/node-daemon/src/sandbox.rs` for the permission
setup).

## Concurrency: one opencode per host

opencode keeps its own SQLite state DB; two concurrent runs on the same host
fail with `database is locked`. Set `AGENTGRID_MAX_CONCURRENCY=1` on any
node whose adapters include `opencode`. Scale by adding hosts, not slots.

## Adding a node

Four ways, in the order to try them. All of them end the same way: the daemon
enrolls with a one-shot token, appears in `Nodes`, and from then on receives
tasks like any other node (delivery is pull — the control plane never dials
it).

**1. One line, run on the new host.** `Nodes → Install node… → get join
command` mints a token and prints it:

```bash
curl -fsSL https://cp.example.com:7800/v1/node/join.sh | sudo bash -s -- --token <token>
```

That is the whole procedure. The host fetches the join script from the control
plane, which downloads the daemon (and the adapters named on the line) from
`/v1/node/bundle`, verifies every byte against the bundle's SHA-256, and hands
the staging directory to the same `deploy/install-node.sh` the release tarball
ships — so this ends in a systemd unit, exactly like option 4.

Nothing on the control plane connects to the host, which is the point: it works
when the CP cannot reach the host at all (NAT, no ssh daemon, a host behind a
bastion). The host only has to reach the CP, which it must do anyway. The
binaries come from `AGENTGRID_NODE_DIST` — the ones this control plane would
have pushed — so a CP that can push can also serve.

The token is single-use and expires in ten minutes. It is passed as an argument
rather than a query parameter on purpose: a query string lands in the access
log, where a token outlives its TTL. Add `--dry-run` to fetch and verify
everything without installing, `--adapters mock,opencode` to pick what to
install, or `--server <url>` when the script was not fetched from the CP.

Add `--probe` to check the path **before** anything is installed:

```bash
curl -fsSL <server>/v1/node/join.sh | sudo bash -s -- --token <t> --probe
```

It runs the daemon it just verified against `probe --server <server>` and stops
if this host cannot reach it. Through a relay the `CONTROL PLANE` line reports
`hops=1` — which is the only way to tell a relay that forwards from a proxy that
merely listens. Works with `--dry-run` too, and needs no root.

**1b. When the host cannot reach the control plane at all**, the line has to
name something the host *can* reach — a sibling node's relay (ADR 0026). Ask
for it by name and the control plane substitutes the address:

```bash
ag nodes add --name behind-nat --via node-a
#   curl -fsSL http://node-a.internal:7900/v1/node/join.sh \
#     | sudo bash -s -- --token <t> --server http://node-a.internal:7900
```

Two things have to be true on the sibling first:

```bash
AGENTGRID_RELAY_LISTEN=0.0.0.0:7900        # carry other nodes' CP traffic
AGENTGRID_RELAY_ADVERTISE=http://node-a.internal:7900
```

`AGENTGRID_RELAY_ADVERTISE` is not optional there. A `0.0.0.0` listen means
"every interface on *this* machine", which is not an address anyone can dial:
without an advertise value the node announces **nothing** rather than guessing
a hostname that would only be wrong on the other machine. The announced URL is
what `ag nodes ls` shows in `RELAY` and what `--via` resolves to.

Every refusal is deliberate:

| what you asked for | what you get |
| --- | --- |
| a node id or name that does not exist | `404` |
| a **revoked** node | `409` — its credential is dead, so it cannot reach the CP and cannot carry a join; the message names `ag nodes rekey` |
| a node that listens but announces nothing | `409` naming `AGENTGRID_RELAY_ADVERTISE` — not the variable you already set |
| a node that is not a relay at all | `409` naming `AGENTGRID_RELAY_LISTEN` |
| an **offline** relay | `200`, with its `status` and `last_heartbeat_at` — a warning, not a refusal: it may be back before you paste the line |
| a token minted for a request that was then refused | nothing — the token is minted *after* the relay resolves |

There is no silent fallback to a direct join. You asked for a relay *because*
the direct path does not work, so a quietly direct command is one that fails
later, on a machine you cannot inspect. The same reasoning is why a revoked
relay is refused rather than offered: it is not a slower path, it is no path.
An offline one is different — it is a warning, and `--probe` (above) turns that
warning into a stop before anything is installed.

**2. From the panel.** `Nodes → Install node…`. The control plane runs the
install itself over ssh: verify (or pin) the host key, copy
`agentgrid-node-daemon`, write a `0600` env file with a one-shot token, start
the daemon. Each step is shown as it completes.

This needs two things on the control plane:

```bash
AGENTGRID_PROVISION_ENABLED=1          # off by default, see ADR 0027
AGENTGRID_NODE_DIST=/opt/agentgrid/bin # where agentgrid-node-daemon lives
AGENTGRID_PUBLIC_URL=https://cp.example.com:7800   # default for the form
```

and an `ssh` client with a path to the host (a key file, or the agent). The
form asks for the **server URL the node will dial** — the control plane, or a
peer node's relay (`AGENTGRID_RELAY_LISTEN`, ADR 0026) when the host cannot
reach the CP directly.

With the flag off, the same dialog renders the server's `reason` and the
equivalent `ag nodes install` command, so nothing is a dead end.

**3. From the CLI**, on a machine that can reach the host:

```bash
ag nodes install --host user@host [--ssh-key ~/.ssh/id_ed25519] \
  --name box-2 --adapters opencode --repositories '*' --max-concurrency 1 \
  --server https://cp.example.com:7800
```

`--server` makes the node dial that URL itself. **Without it the CLI opens a
reverse SSH tunnel to the machine running the command** (`--local-port`, 7800),
which only works when that machine can reach the control plane — handy for a
laptop next to the CP, wrong for anything else. `--accept-new-host-key` is
required for a fresh host (the default refuses an unknown key) and
`--host-key-fingerprint SHA256:…` pins it.

For access granted by a CA rather than a long-lived key, add
`--ssh-cert ~/.ssh/id_ed25519-cert.pub` next to `--ssh-key`: the certificate
travels to `ssh` and `scp` as `-o CertificateFile=…` and is read on the machine
running the command, so the host trusts the CA and the credential expires on
its own. A certificate without `--ssh-key`, or together with `--password`, is
refused before a connection is opened — not silently ignored.

**4. On the host itself**, for a durable install:

```bash
./deploy/install-node.sh --server https://cp.example.com:7800 --token <enroll-token>
```

This creates the unprivileged `agentgrid` user, a systemd unit with the full
hardening set and an `EnvironmentFile` for the token. `ag nodes install` and the
panel do **not** write a systemd unit: they start the daemon with
`setsid nohup`, so it does not survive a reboot. Bootstrap with one, keep it
with the other.

**Fully manual** (no ssh from anywhere, e.g. an air-gapped jump host): mint a
token, copy `agentgrid-node-daemon` + `adapter-*`, and start it:

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

`tests/e2e/remote-ssh.py --file …` uploads without sshpass. Use
`AGENTGRID_ALLOW_ROOT=1` when running as root — nothing sets it for you.

## Re-keying a node (a leaked credential)

`credential.json` holds the node's identity **and** the X25519 key its
adapter-env secrets are sealed to. If a copy of it leaks, revoking and enrolling
again is the wrong repair: enrollment mints a **new node id**, which drops every
ciphertext in `adapter_env_sealed` and leaves you re-entering every secret by
hand. Re-key instead — same node id, same task history, same sealing key:

```bash
ag nodes rekey <node-id>          # prints the token and the command that spends it
```

On the node host, start the daemon **once** with both variables:

```bash
AGENTGRID_REKEY=1 AGENTGRID_ENROLL_TOKEN=<token> <the daemon's usual command>
```

It exchanges the token for a new credential, keeps the sealing key already in
`credential.json`, and scrubs both variables from `AGENTGRID_ENV_FILE`. The token
is single-use, valid 30 minutes, and audited as `node.rekey`; the old credential
stops working the moment it is spent. A **revoked** node comes back this way,
which is the point: `DELETE /v1/nodes/{id}` is otherwise irreversible, and the
replacement would be a different node under the same name.

Two things this deliberately does *not* do. It does not rotate the sealing key —
a leaked credential alone does not compromise it, and rotating would throw away
every secret. And if the **key** did leak, delete `credential.json` (or just its
`key_secret`) before the re-key: the node then mints a new key, the control plane
drops the old ciphertexts, and every secret has to be re-entered with
`ag adapter-env set`. If the re-key fails — expired token, control plane
unreachable — the old file is left untouched and still works.

Re-keying a **healthy** node works too, but the running daemon keeps its old
credential until it is restarted with the token, so do it as part of a planned
restart. And if the restart happens without `AGENTGRID_REKEY=1`, the node is
locked out and needs another re-key — the token is single-use, and the old
credential is already dead.

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
differs from the fleet default). The WebSocket transport is proxied too: the
node opens an HTTP `CONNECT` tunnel and performs the upgrade inside it, so
`AGENTGRID_TRANSPORT=ws` works through the pool. Two requirements for that:

- the proxy must allow `CONNECT` to the control plane (a proxy that only
  forwards absolute-form HTTP requests carries polling and nothing else — the
  node's error names the refusal, e.g. `proxy refused CONNECT cp:7800: 403`);
- a proxy URL without a port is read as **3128** (squid's default) for
  `http://` entries, and credentials in the URL (`http://user:pw@host:port`)
  are sent as `Proxy-Authorization: Basic`.

If a proxy in the pool dies, the node rotates to the next one on the next
connection attempt, exactly as it does for polling.

A proxy is the right tool when the node needs egress *somewhere*. When it has
**no route to the control plane at all** but does reach a sibling node, that
sibling can carry its traffic instead — `AGENTGRID_RELAY_LISTEN` on the
reachable node, `AGENTGRID_SERVER=http://that-node:7900` on the stranded one.
Unlike the proxy pool this carries the WebSocket transport too, and it needs
no cluster token. See "Связь через промежуточный хост" in
`docs/runbook-transport.md` and ADR 0026.

Relays chain without extra configuration: if node B is itself stranded and
carried by A, B can still relay for C — C points at B, B already points at A,
and the request crosses both. Nothing needs to know how long the chain is
except the loop guard, which counts:

```bash
# how many relays one request may cross before a relay answers 508 (default 3)
AGENTGRID_RELAY_MAX_HOPS=5 agentgrid-node-daemon
```

Every relay adds 1 to the `x-agentgrid-relay-hops` header it forwards, and
refuses with `508 Loop Detected` once the count reaches the limit — the body
names the header and the limit, and `agentgrid_edge_loop_detected_total` on
`/metrics` increments. This is the guard for the failure that is otherwise
invisible at setup time: two operators point their relays at each other, both
listen, both accept, and every request that enters the ring is forwarded
forever. The static startup check only catches a relay whose own upstream *is*
itself. `0` is refused at startup (a relay that answers 508 to everything) and
a non-numeric value is an error rather than a silent default.

The default of 3 fits the chain an operator actually builds (C → B → A →
control plane is three) and cuts a ring in milliseconds instead of leaving it
to a timeout. Raising it only makes sense if you have a longer deliberate
chain; the number is a loop bound, not a performance knob.

Both mechanisms produce the same misleading answer: a dead proxy in front of the
control plane and a relay that lost its own upstream both reply `502`, which is
not a transport error, so nothing marks them dead. The node now leaves such an
*endpoint* after three consecutive `502`/`504` — `AGENTGRID_SERVERS` gives it
somewhere to go. That does not mark the *proxy* dead: the pool rotates on
connect/timeout only, so a proxy answering `502` stays in the pool and a node
with a single endpoint has nowhere to move.

Three is a default, not a law:

```bash
# rotate after 5 consecutive 502/504 instead of 3
AGENTGRID_GATEWAY_FAILURES_BEFORE_ROTATING=5 agentgrid-node-daemon
```

The value must be an integer above 1; anything else (`1`, `0`, `three`, empty)
is ignored and the default is used, with a warning in the node log. It is not
allowed to be 1 on purpose: a single `502` is a blip, and rotating on it would
shuffle every node in the fleet during any bad minute. The counter is shared by
poll, heartbeat and ws, so the threshold counts one run across all three.

## Is this node connected directly?

Two mechanisms above make a node reach the control plane *indirectly* — the
egress proxy pool and a relay chain — and they fail in the same way: everything
looks fine, because the node is online. Until this change nothing recorded which
path a node used, so "node B is online" could mean four different things.

`GET /v1/nodes` now carries two fields per node, and the difference between them
is the point:

| field | source | who can be wrong |
| --- | --- | --- |
| `relay_hops` | **observed** — a forwarder stamps `x-agentgrid-relay-hops` on every request it forwards and *replaces* whatever the client sent | a node cannot claim fewer hops than it used |
| `egress_proxy` | **claimed** by the node in its heartbeat | a forward proxy is invisible to the receiver, so the node is the only possible source |

```bash
ag nodes ls
# ... SPOOL  REACH        RELAY                     REPOS
#     -      direct       -                         -
#     -      relay×2      -                         -
#     -      proxy        -                         3/3
#     -      relay×1+proxy -                       3/3
#     -      direct       node-a.internal:7900      3/3   <- carries other nodes
#     -      ?            -                         -     <- no heartbeat since the columns existed
```

`RELAY` is a third fact, and the weakest of the three: `relay_url` is what the
node **claims** other hosts should dial to reach the control plane through it.
The control plane cannot verify it — a relay exists precisely so that the peers
who can reach it are the ones that cannot reach the control plane — so it is
shown as an address, never as a health state. `-` means "not a relay". It is
what `ag nodes add --via` resolves against.

One more state, added because it was the invisible one: `:7900 (no advertise)`.
That node **is** running a relay — `AGENTGRID_RELAY_LISTEN` is set — and
announces no address anyone can dial, because `AGENTGRID_RELAY_ADVERTISE` is
not. It rendered as `-` before, exactly like a node that is not a relay, so the
only advice an operator could find sent them to the variable they had already
set.

`?` is not `direct`: it means no heartbeat has carried either fact yet (a row
older than the migration). The panel renders the same three states as badges.

**Every forwarder stamps, limit or no limit.** `EdgeConfig::max_hops` used to
double as "stamp at all": the standalone `agentgrid-cp-edge` ran with `None`,
left the header alone, and every node behind it read as directly connected —
which is the one question the header exists to answer. The two jobs are now
separate: the limit cuts loops, the stamp makes the hop visible. A standalone
edge reports `1`.

So `relay_hops: 0` means "no AgentGrid forwarder in the path". It does **not**
rule out an L4 balancer or a generic HTTP proxy in front of the control plane:
those do not stamp, and at layer 4 there is nothing to stamp with. Read
`relay×N` as "N AgentGrid forwarders carried this".

Since 0.8.2 every response also echoes the count it observed in
`x-agentgrid-relay-hops`, always written — `0` included. That is what lets a
client check its own path (`agentgrid-node-daemon probe --relay`), and it is
why an *absent* header means "this control plane is too old to echo", never
"zero hops".

The egress-proxy claim is re-evaluated every heartbeat from the pool's *current*
entry, so a pool whose every entry is dead reports `false` on the next beat: the
node has fallen back to direct egress, and the badge disappears with it.

## What can *this host* reach?

Everything above is the control plane's view of a node. The complementary
question — from this host, what is reachable, and through what? — cannot be
answered from there, and it is the one that matters while you are standing on a
host that is not a node yet.

The probe lives in `agentgrid-node-daemon` and needs no credential, no
enrollment and no root:

```bash
agentgrid-node-daemon probe --server https://cp.example.com:7800
agentgrid-node-daemon probe --relay http://node-a.internal:7900 --host db.internal:5432
```

On a host with nothing installed, copy the binary from a sibling (`scp
node-a:/usr/local/bin/agentgrid-node-daemon .`) or fetch it through the relay
you are testing. It is dispatched before the daemon's root check, which is the
only way it can run on a fresh host — exactly when it is most useful.

The binary takes no arguments when it is a daemon (configuration is the
environment), so `--help` prints the usage instead of starting one, and any
other argument is refused by name rather than ignored. That matters on the host
you are probing: an unrecognised flag used to boot the agent, create its data
directory and try to enroll against `127.0.0.1:7800`.

### On a host you can only ssh to

`ag nodes probe` does the copy for you, over the same ssh an install would use —
same key, same certificate, same host-key policy — and removes the binary when
it is done:

```bash
ag nodes probe --host fong@10.0.0.9 \
  --server https://cp.example.com:7800 \
  --relay http://node-a.internal:7900 \
  --tcp db.internal:5432
```

`--server` defaults to the control plane this `ag` is pointed at, the ssh flags
are the ones `ag nodes install` takes, and the exit code is 1 when any check
fails — so a working install line can be reused verbatim, and the result can go
straight into a script.

The binary it copies is the one next to `ag`, or `--binary`. That matters: the
check has to run on *that* host, so the binary must be built for it. An `ag` on
a laptop has no daemon for a linux/x86-64 server, and the failure looks like
"could not execute the probe". Point `--binary` at the bundle the control plane
serves (`/v1/node/bundle/agentgrid-node-daemon`) or at a copy from a sibling.

Output is grouped by question, and the exit code is 1 when any check fails, so
it belongs in a script:

```text
CONTROL PLANE:
  https://cp.example.com:7800  ok     41 ms  v0.8.2 (node protocol v2), hops=0

RELAY:
  http://node-a.internal:7900  ok      8 ms  forwards, hops=1
```

`--relay` is the check with no equivalent anywhere else: it asks that URL to
carry a request *to* a control plane, then reads back the hop count the control
plane observed. `hops=1` means it relayed. A URL that answers with `hops=0` is
listening but **not** relaying — a plain HTTP proxy, an L4 balancer, or the
control plane itself. A response with no hop header at all is not a failure
either: the control plane is then too old to echo, so the probe says it cannot
tell rather than turning every relay red on a rollback.

`--proxy` is opt-in for a reason worth knowing: the daemon honours
`HTTP_PROXY`/`HTTPS_PROXY` for its own outbound traffic, and a probe that
inherited them would measure the *proxy's* reachability. The question is what
this host can reach.

| flag | what it answers |
| --- | --- |
| `--server` | can this host reach a control plane, and which version is it |
| `--relay` | does that URL actually forward to the control plane |
| `--gateway` | is the credential gateway up (`/health` → `ok: true`) |
| `--host` | raw TCP to anything: a sibling's relay port, an ssh endpoint, a database |

All four are repeatable. `--server` defaults to `AGENTGRID_SERVER` /
`AGENTGRID_SERVERS` and `--gateway` to `AGENTGRID_CRED_GATEWAY_URL`, so on a
host that is already a node the bare command checks the configured set. Add
`--json` for scripts, `--timeout <secs>` to change the 5 s per-check limit.

## A host that was configured and never arrived

`nodes` is keyed by enrollment, so a host whose daemon never enrolled has no row
anywhere. The install attempts are the record, and since 0.8.2 they are a table
(`node_installs`) rather than a `HashMap`: a restart used to erase the only
evidence that anyone had tried.

```bash
curl -s "$CP/v1/node-installs" -H "authorization: Bearer $JWT" | jq '.[] | {host, name, state}'
```

`state` is one of four:

| state | what it means |
| --- | --- |
| `issued` | an enrollment token was minted for this host and nothing has been heard since |
| `running` | the control plane is provisioning it over ssh right now |
| `succeeded` | either ssh/scp finished (the panel's install) or the host enrolled (an `issued` row); for the first, **not** "the node is up" — check the `nodes` list |
| `failed` | the install stopped; `error` says where |

`issued` is the one that covers the paths the panel cannot see: `ag nodes
install` runs ssh from *your* laptop, and the install script runs on the host, so
neither ever calls `/v1/node-installs`. Both mint a token first, and since 0.8.2
the mint carries `{host, name}`, which is what creates the row. A row that stays
`issued` forever is a host somebody configured and never got running — exactly
the case that used to be invisible.

An `issued` row closes itself when the host arrives: enrollment looks up the
rows with that name, marks them `succeeded` and adds a step naming the node it
became. Without that, every install that worked would leave a permanently open
"token went out and nothing came back" row, and a list whose normal case never
clears is a list nobody reads. The match is by name because that is the only
thing the two paths share.

The close is a sweep of the table, not of the list. The panel renders the newest
twenty attempts, but the table keeps every one of them, so an intent that has
aged out of the panel is still `issued` on disk — and a close that only touched
what happened to be in memory would leave the durable record claiming a host
never arrived when it is enrolled and heartbeating. Both the in-memory entry and
every matching `issued` row in `node_installs` are closed, on the same single
writer, so the sweep can never run before the row it is closing has been
written.

A row still at `running` when the control plane starts again is shown as
`failed` with "the control plane restarted while this install was running". The
stored state is left alone on purpose: on an HA standby that row belongs to the
leader, and a replica that "fixed" it would replicate the fix over a live job.

Finished attempts are deleted automatically after 30 days
(`AGENTGRID_INSTALL_RETENTION_HOURS`; `0` keeps every attempt forever). Only
`succeeded` and `failed` are eligible. An `issued` row is the answer to "was
this host ever configured?" and a `running` row is the only trace of an install
whose outcome nobody learned, so both are kept however old they are — which is
also what bounds the table: one open intent per host that has not arrived. That
"one" is enforced by the table (a partial unique index on `(host, name)` where
`state = 'issued'`), not by the panel's job list: the list holds twenty entries,
so a guard that read it stopped guarding exactly when it was busy. A second mint
for a host that already has an open intent writes nothing, and says so by
returning 200 with the same token as any other mint.

A host that is gone for good is removed explicitly: `DELETE
/v1/node-installs/{id}`, `ag nodes forget <id>`, or `forget` in the panel. The
pair matters — `DELETE /v1/nodes/{id}` revokes a node that *did* enroll and
keeps its history; this deletes the record of a host that never became one, and
it is the only way out for a decommissioned host, because `issued` rows never
age out. A `running` row is refused with 409: the control plane is installing
right now, and that row is the only place it shows.

`ag token create` passes no host and records nothing — it hands a token to a
human who enrolls a machine by hand, and inventing a host record for that would
be a lie.

## Are the control-plane replicas talking to each other?

`GET /v1/ha/status` answers that, but it is behind the cluster token, which no
panel has. `GET /v1/cluster/status` is the same body for an operator (JWT,
read-only), and `ag cluster status` renders it:

```
node=cp-a role=leader term=3 epoch=1 applied_seq=91 leader=cp-a

PEER         ROLE     STATE    AGE      SEQ      URL
cp-b         data     up       1s       91       http://cp-b:7800
cp-c         data     down     4m       88       http://cp-c:7800
cp-d         witness  never    -        -        http://cp-d:7800
```

The liveness comes from the exchanges consensus already makes — the leader's
heartbeat fan-out, an inbound heartbeat, a vote request — not from a separate
probe, so there is nothing extra to configure and no probe that can disagree
with the failure detector. `state`:

- `up` — the last exchange succeeded.
- `stale` — one miss, or no contact for two election timeouts.
- `down` — repeated failures.
- `never` — no exchange yet.

Configuration is the skeleton: a peer that has never answered is still listed,
because unreachable and nonexistent are different problems. The list is a view
from **one** replica — only the leader fans out to every peer, so a standby
legitimately shows `never` for most of them. `applied_seq` is the peer's last
reported replication position and survives failures: a peer that stopped
answering is behind by at least that much, which is what tells you whether it
could safely take over.

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
