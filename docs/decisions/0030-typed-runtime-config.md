# ADR 0030: one typed runtime configuration for both daemons

Status: accepted (2026-11-27)

## Context

The control plane and the node daemon each read their own environment
variables, in their own code, with their own defaults:

- control plane: `AGENTGRID_LISTEN`, `AGENTGRID_DB`, `AGENTGRID_ARTIFACT_ROOT`
  (`crates/control-plane/src/main.rs`)
- node daemon: `AGENTGRID_SERVER`, `AGENTGRID_DATA_DIR`,
  `AGENTGRID_NODE_NAME`, `AGENTGRID_WORKSPACE_ROOT`,
  `AGENTGRID_REPOSITORY_ROOT`, `AGENTGRID_MAX_CONCURRENCY`,
  `AGENTGRID_HEARTBEAT_SECS`, `AGENTGRID_ADAPTERS`, `AGENTGRID_TRANSPORT`
  (`crates/node-daemon/src/config.rs`)

Three consequences showed up in production incidents and reviews:

1. **A bad value was silently ignored.** `AGENTGRID_MAX_CONCURRENCY=two`
   parsed to `None` and fell back to `2`; `AGENTGRID_TRANSPORT=websockets`
   fell back to `auto`. The daemon started with a value nobody asked for.
2. **Nothing tied a production deployment to a durable path.** The operator
   could point `AGENTGRID_DB` at `/tmp` and the daemon would happily write
   there. In the deleted-temp-directory incident, that is exactly what
   happened: the database lived in a scratch directory that somebody removed,
   and the control plane kept running against a deleted file.
3. **The two daemons could not share one config file.** There was no typed
   structure to write, so an operator had to duplicate a path block into
   every unit file and keep the copies in sync by hand.

## Decision

One typed configuration in `crates/common/src/runtime_config.rs`, used by the
control plane, the node daemon and the CLI.

- `AGENTGRID_CONFIG` points at a YAML file with `control_plane` and `node`
  sections. Unknown fields are rejected (`deny_unknown_fields`), so a typo in
  the file is a startup error instead of a silently ignored setting.
- Explicit environment variables override file values. Existing deployments
  keep working unchanged; the file is an addition, not a replacement.
- Invalid values fail at startup: a non-integer concurrency, a transport
  outside `poll|ws|auto`, an empty adapter list.
- In production (`AGENTGRID_ENV=production`) every runtime path must live
  under `/var/lib/agentgrid`. The check resolves `.`/`..` and follows symlinks
  on the deepest existing ancestor before comparing, so neither a sibling
  directory (`/var/lib/agentgrid-evil`), a `..` escape, nor a symlink planted
  inside the root can pass as a production path.
- The production flag comes from the existing `DeployEnv` resolution, so
  there is exactly one answer to "is this production" in the process.

## Consequences

- The control plane and node daemon now fail fast on a broken config instead
  of starting with different values than the operator wrote down.
- A production install can no longer keep its state in a scratch directory
  by accident.
- Two environment readers collapse into one; the remaining per-daemon
  variables (`AGENTGRID_JWT_SECRET`, `AGENTGRID_ENROLL_TOKEN`, HA settings)
  stay where they are, because they are secrets or deployment topology, not
  runtime configuration.
- The YAML file is optional. An operator who keeps using environment
  variables only sees the stricter validation.

## Alternatives considered

- **Validate environment variables in place, without a file.** Cheaper, but it
  leaves the two daemons with duplicated defaults and no shared structure, and
  the operator still has to duplicate paths into every unit file.
- **TOML instead of YAML.** The repository already parses YAML workflows
  with `serde_saphyr`, so YAML costs no new dependency and matches what
  operators already write.
- **Read the file only in the CLI and pass the result down.** The CLI is not
  the only entry point — systemd units and Docker Compose start the daemons
  directly — so the daemons must be able to read the file themselves.
