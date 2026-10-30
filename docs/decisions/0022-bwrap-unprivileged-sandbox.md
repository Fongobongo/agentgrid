# ADR 0022: Unprivileged namespace sandbox (bubblewrap)

Status: accepted (backlog item 1 of `docs/ROADMAP.md`)

## Context

`SandboxKind` had two isolation backends and both have a hard prerequisite:

- `docker` needs a container runtime installed and a reachable daemon;
- `systemd` (transient scope) needs a *user* manager (`systemctl --user`
  answering, i.e. lingering enabled) and, for `AGENTGRID_RUN_AS_USER`, root
  to call `runuser`.

A node with neither — a plain VPS, a CI runner, a Termux device, a dev
laptop without Docker — falls back to `SandboxKind::None`, where the agent is
a direct child of the daemon with the host's filesystem, pid space and
network. A review of 878 starred repositories flagged this as the top gap;
`grep` confirmed no `landlock`, `seccomp`, `nsjail` or `bwrap` anywhere in the
tree. The threat model already assumes the node is the isolation boundary
(`docs/decisions/threat-model.md`), so "no backend available" silently meant
"no boundary at all".

## Decision

Add a third *unprivileged* backend: `SandboxKind::Bwrap`
(`AGENTGRID_SANDBOX=bwrap|bubblewrap|userns`). It needs only unprivileged user
namespaces, which are enabled by default on Debian 12 / Ubuntu 22.04+, so it
covers exactly the hosts the other two cannot.

The argv (built by `bwrap_head`, both spawn paths route through it):

```
bwrap --die-with-parent --new-session --unshare-all [--share-net]
      --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp
      (--bind | --ro-bind) <worktree> <worktree> --chdir <worktree>
      [--uid N [--gid N]] [--setenv HTTP(S)_PROXY …] -- <program> <args…>
```

Four decisions are worth recording:

1. **`--unshare-all` is not optional.** Mount, pid, ipc, uts and cgroup
   namespaces are the boundary; a bwrap invocation that only bind-mounts is a
   decoration, not a sandbox. `--share-net` is the single exception, and only
   for modes that are allowed egress.
2. **Network mapping is coarser than docker's** — bwrap can only share or
   unshare the whole netns. `none` → unshared; `unrestricted` → shared;
   `restricted` → shared **only** when `AGENTGRID_SANDBOX_EGRESS_PROXY` is
   set, because the proxy is then the filter (a host process reaches it by
   URL, so `AGENTGRID_SANDBOX_EGRESS_NETWORK` — a docker-network name — is not
   required). An unknown *task*-supplied mode clamps to unshared, same
   fail-closed rule as `docker_run_head`. An `allowlist:` CIDR spec stays
   refused at startup: no backend can enforce per-CIDR egress.
3. **`AGENTGRID_RUN_AS_USER` must be numeric** (`<uid>[:<gid>]`) — `bwrap
   --uid` does not resolve names. A named spec is refused at startup rather
   than ignored: a daemon must not believe it separated identities when it
   did not. uid 0 is refused for the same reason as elsewhere.
4. **No cgroup limits.** `ResourceLimits` and the
   `AGENTGRID_SANDBOX_{PIDS_LIMIT,MEMORY,CPUS}` knobs remain docker/systemd
   features; the supervisor's RSS budget poll still bounds the attempt. The
   heartbeat reports `sandbox_backend = "bwrap"` and `enforced_limits = false`
   so the control plane never over-claims.

Two consequences fall out of running on the host rather than in a container:

- **The child must not inherit the daemon's environment.** `Docker`/`Systemd`
  children are started from a different environment and need it forwarded;
  `None` and `Bwrap` children inherit the spawn env. `SandboxKind::
  inherits_daemon_env()` encodes this and the eval probe uses it, so an
  agent-influenced `sh -c` cannot read the CP token or a git credential.
- **Startup is fail-closed.** `probe_bwrap` resolves the binary *and* creates
  a real namespace (`bwrap --unshare-all --ro-bind / / -- true`). A kernel
  with `kernel.unprivileged_userns_clone=0`, or a seccomp policy that blocks
  `unshare`, fails the daemon at start instead of failing every attempt. A
  root daemon with no `AGENTGRID_RUN_AS_USER` logs a warning: the namespaces
  still confine the agent, but bwrap alone is not a privilege drop.

## Proof

Run against bubblewrap 0.8.0 on Debian 12 / kernel 6.1 as uid 1001 (the argv
above, `--unshare-all` without `--share-net`):

```
id -u                 → 1001
echo hi > ok.txt      → WRITABLE          (worktree bind, on top of a /tmp tmpfs)
ls /usr               → ROOT_OK           (host root visible, read-only)
ip -o link | wc -l    → 1                 (loopback only)
curl https://…        → net=BLOCKED
touch /etc/x          → ROOT_RO
```

With `--share-net`: 10 links, `net=200`. With `--ro-bind` on the worktree:
`WORKTREE_RO`. Unit coverage (`crates/node-daemon/src/sandbox.rs`) asserts the
argv itself — flags, bind order, `--` terminator, network mapping, numeric-only
run-as, and that `inherits_daemon_env()` is false for `None`/`Bwrap` — so the
contract holds on the Windows CI runners where bwrap does not exist.

## Consequences
- Hosts that previously had to choose between "install Docker" and "no
  sandbox" get a mount/pid/net/ipc boundary with one `apt install bubblewrap`.
- The backend is argv-only, so it is fully unit-tested on every platform (the
  daemon's own CI runs on Windows, where the flags are just strings).
- No syscall filtering: bwrap is invoked without `--seccomp`, so the agent
  keeps the host's syscall surface. Docker's default seccomp profile is
  stricter on that axis; `bwrap` is the availability/privilege trade, not a
  replacement for a hardened docker node.
- No per-attempt naming: cancel relies on `--die-with-parent` plus the
  existing process-group kill, so there is nothing to clean up after a hard
  daemon crash (unlike orphaned containers/scopes).

## Revisit when

A syscall filter is wanted on hosts without a runtime: `bwrap --seccomp <fd>`
accepts a BPF program, which would make `bwrap` + a generated allowlist the
first backend with per-agent seccomp (ADR 0017 named that as the missing
"strong isolation" axis).
