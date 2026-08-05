# ADR 0005: Docker sandbox isolation — enforceable subset + fail-closed

Status: accepted (Milestone 4, §25–§27 hardening)

## Context

The node daemon can run an attempt's agent inside a Docker/Podman container
(`AGENTGRID_SANDBOX=docker`) instead of a bare subprocess. The hardening plan
asks for real isolation: no host mounts, cap-drop, no-new-privileges, network
isolation, resource limits, and — critically — **capability honesty**: never
claim an isolation the runtime does not actually apply.

The constraint that drives every decision below: **the docker CLI has no
native per-connection or per-CIDR egress filter.** `--network none` is
all-or-nothing (no egress), a user-defined bridge network NATs to the host's
full egress, and `--internal` blocks all egress too. "Internet but no LAN" and
"allowlist of destinations" cannot be expressed with `docker run` flags alone.

## Decision

1. **Task network modes map to docker-native networks.**
   `none` → `--network none`; `unrestricted` → `--network bridge`; `restricted`
   → `--network none`. `restricted` promised "no LAN/private ranges", which
   docker cannot express as "internet but no LAN", so it resolves to the
   strictest enforceable mode (no network at all) — **strictly more isolated
   than promised, never less**. Previously a raw `--network restricted` was
   passed to docker and simply failed the container at start.

2. **Egress allowlisting fails closed.** `AGENTGRID_SANDBOX_NETWORK=allowlist:<cidrs>`
   is syntactically validated (typo'd CIDRs die at startup, not mid-attempt)
   but the daemon **refuses to start** with it: running would silently mean
   full egress while the operator believes egress is allowlisted. The
   documented upgrade path is an egress proxy (allowlisted destinations only)
   wired as the sandbox's network.

3. **Every container is stamped with its owner.** `--label agentgrid.node=<node_id>`
   on every `docker run`, and the daemon removes its own orphaned containers
   at startup (`docker ps -aq --filter label=…` + `docker rm -f`). A
   SIGKILLed daemon strands attached containers (`--rm` only fires on clean
   exits); this reclaims them.

4. **Adapters are smoke-tested inside the image.** `probe_adapter_in_sandbox`
   runs `docker run --rm --entrypoint sh <image> -c 'command -v <bin>'` per
   adapter at startup; a missing in-image adapter marks the node degraded so
   the scheduler excludes it. The host-side `probe_adapter` proves nothing
   about what the container ships.

5. **Artifacts get a separate writable mount.** `AGENTGRID_SANDBOX_ARTIFACT_DIR`
   mounts a host dir read-write at `/artifacts`, independent of `--read-only`,
   so a read-only worktree still has a writable output place.

6. **Capability reports are honest.** The heartbeat's `enforced_limits` is
   true only when Docker is active, resource limits are set, AND the effective
   network resolves to `none`. A bridge/unrestricted policy means egress is
   not isolated, so the flag turns false. The node's declared `network_mode`
   (heartbeat) stays the raw policy ceiling — the scheduler rank-compares
   task mode against it (none < restricted < unrestricted); the *resolved*
   docker network is logged per-attempt (egress audit) instead.

7. **Egress audit is configuration-level, not per-connection.** Each attempt
   logs `task_network_mode` + `resolved_network` + `sandbox` so operators can
   verify the deployed policy from daemon logs. Per-connection audit requires
   the egress-proxy upgrade path (proxy access logs).

## Consequences

- `restricted` tasks lose all network today (they gained nothing before — the
  raw mode broke docker). Operators needing LAN-blocking-with-internet must
  deploy the egress proxy; until then `restricted` == `none` is the safe,
  honest behavior.
- Metadata endpoints (169.254.169.254) are unreachable for `none`/`restricted`;
  `unrestricted` (bridge) can reach them — documented tradeoff, same proxy
  upgrade path.
- `enforced_limits=false` for a node whose max policy is `unrestricted` may
  surprise operators who set limits but allow bridge tasks; it is the honest
  report (that node runs unisolated containers).
- All decisions are testable without a docker host: arg-vector assertions
  (`docker_wraps_command`, `task_network_mode_maps_to_docker_native_networks`,
  `docker_artifact_mount_*`), startup validation
  (`allowlist_fails_closed_at_startup`), and label stamping
  (`docker_wraps_command`).

## Removal plan

- **Egress proxy replaces 1, 2, 7:** once the proxy exists, `restricted`
  maps to the proxy network (LAN-blocking with allowlisted internet), the
  `allowlist:` refusal is lifted to a real network, and egress audit becomes
  per-connection proxy logs. Keep the fail-closed refusal until then — it is
  the guard that prevents a false security promise.
- **cgroup v2 backend (plan §26) is a sibling, not a replacement:** a
  systemd transient-scope backend enforces `MemoryMax`/`CPUQuota`/`TasksMax`
  without containers; the Docker label/orphan logic is unaffected.
- Removing the `agentgrid.node` label would break orphan cleanup (decision 3);
  keep it while any `docker run` path exists.
- Decision 5 (artifact mount) can be dropped if the adapter contract later
  declares a single writable root; keep the env var as the compatibility
  path until then.

## Threat-model delta

- **New**: container stamping + orphan reaping reduce the "stranded container
  runs forever after daemon kill" resource-exhaustion vector.
- **Removed**: the false promise of egress allowlisting (was: silent full
  egress under `allowlist:`; now: startup refusal).
- **Unchanged**: cap-drop/no-new-privileges/read-only/mount policy; the proxy
  upgrade path is the remaining gap for LAN filtering and per-connection
  audit, tracked in the hardening plan (§25/§27).
