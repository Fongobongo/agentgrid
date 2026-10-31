# ADR 0027: Installing a node from the control plane

Status: accepted

## Context

The control plane never dials a node. Delivery is pull: a node polls, the
control plane assigns inside the same transaction, and `nodes` has no address
column at all. That is why a node behind a firewall works at all, and it is the
property ADR 0026's relay leans on.

Onboarding is the one thing that cannot be pull. A host that is not a node yet
has nothing to poll with, so somebody has to put a binary on it, hand it a
one-time token and start it. Two things existed for that:

- `ag nodes install --host user@host` — provisions over SSH, including a
  reverse tunnel by default;
- `deploy/install-node.sh` — a systemd unit with hardening, run on the host.

Neither was reachable from the web panel, which offered exactly one button:
"Enrollment token". Read from the panel, the product says "installing a node is
manual". That is not true, and the operator who believes it will not find the
command, because `docs/agent-ops.md` documents only the hand-rolled upload.

## Decision

Put the install behind a button, and make the button honest about what it can
and cannot do.

1. **One implementation.** The SSH path moves out of `crates/cli` into
   `crates/provision` (`Target`, `Plan`, `Transport`, `install`). The CLI maps
   its clap args onto it; the control plane maps a JSON body onto it. Host-key
   policy, argv construction and the `0600` env file exist once. A second copy
   would be a second place to get fail-closed wrong, and the threat model
   already credits this code with refusing an unknown host key (T25).

2. **Opt-in, not default.** `AGENTGRID_PROVISION_ENABLED=1` is required. A
   control plane that can SSH into arbitrary hosts is a different threat model
   from one that cannot: the CP becomes a privileged execution path, and the
   operator has to say yes to it. Without the flag the API answers `403` naming
   the variable, and the panel renders that sentence instead of a form.

3. **A job, not a request.** `POST /v1/node-installs` returns `202` and a job
   id; the install runs on `spawn_blocking` (it spawns `ssh`/`scp`) and appends
   one line per step. `GET /v1/node-installs/{id}` and `GET /v1/node-installs`
   follow it. Jobs live in memory, capped at 20, and a running job is never
   evicted — the panel is polling that id.

4. **Credentials are used, never kept.** A password or key path arrives in the
   request and lives for the length of the install. `InstallRequest` does not
   derive `Debug`, so a stray `{:?}` cannot log it; the audit row is
   `node.install` with the host as the subject and the job id in the payload,
   and it is written before anything runs.

5. **Direct transport only.** The CLI's default is a reverse tunnel to the
   machine running the command. Behind a button that machine is the control
   plane, so the tunnel would be `ssh -R` from the CP to the host and back
   through the CP — an extra hop that adds a supervision problem and solves
   nothing. The install therefore requires a server URL the **node** can dial
   (the CP itself, or a peer's relay from ADR 0026), defaulting to
   `AGENTGRID_PUBLIC_URL`.

6. **The panel always offers the action.** When provisioning is disabled or the
   daemon binary is missing, the modal shows the server's `reason` and the exact
   `ag nodes install` command built from the same form. The command always
   carries `--server`, so a copy-paste never silently opens a tunnel.

### Why not store SSH credentials in the control plane

That is a secrets store with rotation, scoping and its own audit — a different
feature with a different threat model, and one that makes the CP a standing
target rather than a bootstrap path. Key paths point at files on the CP host
and, with nothing configured, ssh falls back to its agent.

### Why not an agent-pull bootstrap

A one-liner the host runs (`curl … | sh` with an embedded token) removes SSH
entirely and is the right long-term answer for cloud hosts. It is also the
fallback the panel already renders, and it needs a signed, versioned bootstrap
script — a supply-chain surface this ADR does not take on.

## Consequences

- The control plane needs three things it did not need before: an `ssh` client,
  a network path to the hosts (port 22, or whatever `--host` names), and
  `agentgrid-node-daemon` next to its own binary (`AGENTGRID_NODE_DIST`). All
  three are reported by `GET /v1/provisioning` before anyone fills in a
  form.
- **Windows control planes cannot use it**: the start step is
  `setsid nohup bash -c …`, and the provisioning tests that spawn a fake ssh
  are `#[cfg(unix)]`. The failure is explicit, not silent.
- It is a **bootstrap**, not a durable install: no systemd unit, no rollback,
  no idempotency beyond `mkdir -p` and overwrite. A host that must survive a
  reboot wants `deploy/install-node.sh` (or the same unit written by hand),
  which the install does not replace.
- Jobs are in memory. A control plane restart loses the job list; the nodes
  that already installed keep running, because nothing about them depends on
  the job.
- The audit records the **request**, not the outcome. A failed install leaves
  one `node.install` row and a `failed` job; there is no second audit row.
- The relay composes: `server` can point at a peer's relay, so a host that can
  only be reached from one node is still installable from the panel — the CP
  reaches it over ssh, and the node reaches the CP through the relay.

## Revisit when

- **The install must be durable.** Writing the systemd unit from the CP (the
  one `deploy/install-node.sh` writes) turns bootstrap into configuration
  management, and then rollback and idempotency become requirements rather than
  non-goals.
- **Credentials need to be per-host.** Today one key path or one agent serves
  every install. Per-host keys mean a vault, and a vault means deciding what
  the CP is allowed to decrypt.
- **A signed bootstrap script exists.** Then the button can hand out a
  one-liner instead of an ssh session, and the flag, the job store and the
  credential path all disappear.
