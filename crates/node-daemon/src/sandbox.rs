//! Agent isolation (idea: sandcastle-style sandbox abstraction).
//!
//! A `Sandbox` wraps the command the node would run the agent as, so an agent
//! can be confined to a container (Docker/Podman/microVM) or a systemd
//! transient scope (cgroups v2) instead of sharing the node's full
//! environment. The default `NoSandbox` runs the agent directly in the
//! worktree (legacy behavior). Configured via `AGENTGRID_SANDBOX`
//! (`none` | `docker` | `systemd`) and `AGENTGRID_SANDBOX_IMAGE`.
//!
//! `sandbox_command` returns the `(program, args)` to spawn: either the raw
//! command, a hardened
//! `docker run --rm -i --entrypoint "" --cap-drop=ALL … <image> <cmd>`
//! prefix (`--entrypoint ""` clears any image ENTRYPOINT so the explicit
//! command wins), or a
//! `systemd-run --user --scope --unit agentgrid-<attempt> --property … <cmd>`
//! prefix (Plan 6.11: native cgroups v2 limits on a Tier-1 host without a
//! container runtime). Both the wrapper path and the ACP path route through
//! it. Docker hardening knobs (plan §25): `AGENTGRID_SANDBOX_NETWORK`
//! (default `none`),
//! `AGENTGRID_SANDBOX_READ_ONLY=1` (read-only root + tmpfs `/tmp`),
//! `AGENTGRID_SANDBOX_PIDS_LIMIT`, `AGENTGRID_SANDBOX_MEMORY`,
//! `AGENTGRID_SANDBOX_CPUS`, `AGENTGRID_SANDBOX_IMAGE_DIGEST` (pin by digest).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxKind {
    None,
    Docker,
    /// Plan 6.11: run the attempt inside a systemd transient scope
    /// (`systemd-run --user --scope`) so the whole agent tree lands in one
    /// cgroup and the kernel enforces MemoryMax / CPUQuota / TasksMax. No
    /// container runtime required — the native Tier-1 isolation backend.
    Systemd,
}

impl SandboxKind {
    pub fn from_env() -> Self {
        match std::env::var("AGENTGRID_SANDBOX")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "docker" | "podman" => SandboxKind::Docker,
            "systemd" | "systemd-scope" | "cgroups" => SandboxKind::Systemd,
            _ => SandboxKind::None,
        }
    }
}

/// Deterministic per-attempt sandbox container name (audit ND-6): enables
/// `remove_sandbox_container` to kill the actual container on
/// cancel/timeout — killing the attached `docker run` client leaves the
/// container running (see `docker_run_head`).
pub fn container_name(attempt_id: &str) -> String {
    format!("agentgrid-{attempt_id}")
}

/// Plan 6.11: deterministic per-attempt transient-scope unit name. The
/// `--user` manager namespaces units per user, so the same name in two
/// concurrent daemons (different `agentgrid` users) cannot collide; within
/// one daemon the attempt id is unique. `systemctl --user kill <unit>`
/// reaches every process the scope holds — the whole agent tree, not just
/// the direct child.
pub fn scope_unit(attempt_id: &str) -> String {
    // Unit names must stay in [a-zA-Z0-9:_.\\-] — attempt ids are
    // `[a-zA-Z0-9-]` by construction (CP-generated), but be safe for any
    // future id shape so an invalid unit name never reaches systemctl.
    let safe: String = attempt_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("agentgrid-scope-{safe}")
}

/// Build the `systemd-run --user --scope --unit … --property …` head
/// (Plan 6.11). `--user` runs the scope under the daemon's own user manager
/// (the `agentgrid` systemd user from install-node.sh) — no root, no
/// polkit rules needed; `--scope` attaches ALREADY-RUNNING processes: the
/// command itself is exec'd by us (the daemon), so stdin/stdout pipes stay
/// ours and the existing EventSink stream keeps working unchanged.
///
/// Resource limits map to the cgroups v2 properties: `MemoryMax`,
/// `CPUQuota` (percent of one core; 200% = 2 cores), `TasksMax`. Per-attempt
/// profile limits (Stage 12 / ADR 0003) override the node-wide env knobs —
/// same precedence as the docker path.
fn systemd_run_head(
    unit: &str,
    limits: Option<&agentgrid_adapters::ResourceLimits>,
) -> Vec<String> {
    let mut v = vec![
        "run".to_string(),
        "--user".to_string(),
        "--scope".to_string(),
        "--unit".to_string(),
        unit.to_string(),
    ];
    let limits = limits.cloned().unwrap_or_default();
    let mem = limits
        .memory_max
        .map(|b| format!("{}M", (b / (1024 * 1024)).max(1)))
        .or_else(|| env_nonempty("AGENTGRID_SANDBOX_MEMORY"));
    if let Some(m) = mem {
        v.push(format!("--property=MemoryMax={m}"));
    }
    let quota = limits
        .cpu_quota_percent
        .map(|c| format!("{c}%"))
        .or_else(|| {
            env_nonempty("AGENTGRID_SANDBOX_CPUS").map(|c| {
                // Reuse the same "% = cores×100" convention as the docker path
                // when the operator pinned cores (e.g. "1.5" → 150%).
                c.parse::<f64>()
                    .ok()
                    .map(|cores| format!("{}%", (cores * 100.0).round() as u64))
                    .unwrap_or(c)
            })
        });
    if let Some(q) = quota {
        v.push(format!("--property=CPUQuota={q}"));
    }
    let tasks = limits
        .tasks_max
        .map(|n| n.to_string())
        .or_else(|| env_nonempty("AGENTGRID_SANDBOX_PIDS_LIMIT"));
    if let Some(t) = tasks {
        v.push(format!("--property=TasksMax={t}"));
    }
    v
}

/// Plan 6.11: kill every process in the attempt's transient scope
/// (`systemctl --user kill <unit>`), then stop the unit so a dead scope
/// does not linger in the user manager. Best-effort — the process-group
/// SIGTERM/SIGKILL from `terminate_group` still applies to the direct
/// child, and the startup sweep (`cleanup_orphan_scopes`) is the backstop.
pub async fn kill_scope_unit(attempt_id: &str) {
    let sandbox = std::env::var("AGENTGRID_SANDBOX").unwrap_or_default();
    if !matches!(
        sandbox.trim().to_ascii_lowercase().as_str(),
        "systemd" | "systemd-scope" | "cgroups"
    ) {
        return;
    }
    let unit = scope_unit(attempt_id);
    for args in [["kill", &unit], ["stop", &unit]] {
        match tokio::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .await
        {
            Ok(o) if o.status.success() => {}
            Ok(o) => tracing::warn!(
                attempt_id,
                "systemctl --user {} {} failed: {}",
                args[0],
                unit,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => tracing::warn!(attempt_id, "systemctl --user spawn failed: {e}"),
        }
    }
}

/// Plan 6.11: after a systemd-scope attempt exits, ask the user manager
/// whether the kernel OOM-killed the scope's cgroup (memory.max breach).
/// `systemctl --user show -p OOMKill --value <unit>` answers
/// `1`/`0`; a failed/absent answer yields None (caller keeps the plain
/// exit-code classification). Upgrades exit-137 into the first-class
/// `resource_limit:memory` error code, mirroring
/// [`inspect_container_oom`] for the docker path.
pub async fn inspect_scope_oom(attempt_id: &str) -> Option<String> {
    let sandbox = std::env::var("AGENTGRID_SANDBOX").unwrap_or_default();
    if !matches!(
        sandbox.trim().to_ascii_lowercase().as_str(),
        "systemd" | "systemd-scope" | "cgroups"
    ) {
        return None;
    }
    let unit = scope_unit(attempt_id);
    let out = tokio::process::Command::new("systemctl")
        .args(["--user", "show", "-p", "OOMKill", "--value", &unit])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stdout == "1" {
        let limit = std::env::var("AGENTGRID_SANDBOX_MEMORY").unwrap_or_default();
        Some(if limit.is_empty() {
            "memory".to_string()
        } else {
            format!("memory (limit {limit})")
        })
    } else {
        None
    }
}

/// Plan 6.11: sweep this daemon's leftover scopes at startup — a SIGKILLed
/// daemon can leave `agentgrid-scope-*` units registered in the user
/// manager. Lists failed/running units matching the prefix and stops them
/// (best-effort, like `cleanup_orphan_containers`).
pub async fn cleanup_orphan_scopes() {
    let out = match tokio::process::Command::new("systemctl")
        .args(["--user", "list-units", "--all", "--plain", "--no-legend"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::warn!(
                "orphan-scope scan failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "orphan-scope scan spawn failed");
            return;
        }
    };
    let stale: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let name = l.split_whitespace().next()?;
            (name.starts_with("agentgrid-scope-")).then(|| name.to_string())
        })
        .collect();
    if stale.is_empty() {
        return;
    }
    let mut cmd = tokio::process::Command::new("systemctl");
    cmd.arg("--user").arg("stop").args(&stale);
    match cmd.output().await {
        Ok(o) if o.status.success() => {
            tracing::info!(count = stale.len(), "removed orphan agentgrid scopes");
        }
        Ok(o) => tracing::warn!(
            "orphan-scope removal failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => tracing::warn!(error = %e, "orphan-scope removal spawn failed"),
    }
}

/// Stage 12 / ADR 0003: after a sandboxed attempt exits, ask the runtime
/// whether the container was OOM-killed (`docker inspect -f {{.State.OOMKilled}}`).
/// Upgrades a bare exit-137 into the first-class `resource_limit` terminal
/// outcome so the CP can treat OOM differently from an agent crash (e.g. never
/// auto-retry). `None` when not sandboxed, the container is already gone
/// (raced removal / non-docker runtime quirk), or the answer is inconclusive —
/// the caller keeps the plain exit-code classification then.
/// Plan 6.11: a systemd-scope attempt routes to [`inspect_scope_oom`].
pub async fn inspect_container_oom(attempt_id: &str) -> Option<String> {
    let sandbox = std::env::var("AGENTGRID_SANDBOX").unwrap_or_default();
    if sandbox.is_empty() || sandbox == "none" {
        return None;
    }
    if matches!(
        sandbox.trim().to_ascii_lowercase().as_str(),
        "systemd" | "systemd-scope" | "cgroups"
    ) {
        return inspect_scope_oom(attempt_id).await;
    }
    let runtime =
        std::env::var("AGENTGRID_SANDBOX_RUNTIME").unwrap_or_else(|_| "docker".to_string());
    let name = container_name(attempt_id);
    let out = tokio::process::Command::new(&runtime)
        .args(["inspect", "-f", "{{.State.OOMKilled}}", &name])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stdout == "true" {
        let limit = std::env::var("AGENTGRID_SANDBOX_MEMORY").unwrap_or_default();
        Some(if limit.is_empty() {
            "memory".to_string()
        } else {
            format!("memory (limit {limit})")
        })
    } else {
        None
    }
}

/// Best-effort teardown of the attempt's sandbox backend: `rm -f` the docker
/// container, and for a systemd scope `kill_scope_unit` SIGTERMs the whole
/// cgroup and stops the unit. No-op when the daemon is not running a
/// container/scope sandbox (mirrors the env the startup config parsed
/// `SandboxKind` from). Failures only warn: the startup orphan sweep
/// remains the backstop.
pub async fn remove_sandbox_container(attempt_id: &str) {
    let sandbox = std::env::var("AGENTGRID_SANDBOX").unwrap_or_default();
    if matches!(
        sandbox.trim().to_ascii_lowercase().as_str(),
        "systemd" | "systemd-scope" | "cgroups"
    ) {
        kill_scope_unit(attempt_id).await;
        return;
    }
    if sandbox.is_empty() || sandbox == "none" {
        return;
    }
    let runtime =
        std::env::var("AGENTGRID_SANDBOX_RUNTIME").unwrap_or_else(|_| "docker".to_string());
    let name = container_name(attempt_id);
    match tokio::process::Command::new(&runtime)
        .args(["rm", "-f", &name])
        .output()
        .await
    {
        Ok(o) if o.status.success() => {}
        Ok(o) => tracing::warn!(
            attempt_id,
            "sandbox container rm -f failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => tracing::warn!(attempt_id, "sandbox container rm -f spawn failed: {e}"),
    }
}

/// Prefix args + program to run `program ...` inside the configured sandbox,
/// rooted at `workdir`. Used by the legacy wrapper-binary spawn path (Stage
/// 11.2 / line 358): `SpawnRequest { bin: program,
/// sandbox_prefix_args }` then appends `--prompt <prompt>`. `None` → no
/// prefix (passthrough as before); `Docker` → `docker run --rm -i -v … -- <image>`
/// with `program` placed inside the container after `--`.
///
/// `sandbox_command` (for the ACP path) keeps returning the fullwrapped
/// `(program, args)` already including `program`; this variant splits them
/// because the legacy ExecutionBackend appends its own `--prompt` after the
/// prefix.
///
/// `limits` is the per-attempt profile ceiling (Stage 12 / ADR 0003): each
/// set field overrides the node-wide env knob for this run; unset fields fall
/// back to the env default.
// Audit X-N3b (same rationale as sandbox_command): the assembly knobs are the
// documented shape; a config struct would churn every caller for no gain.
#[allow(clippy::too_many_arguments)]
pub fn sandbox_prefix(
    kind: SandboxKind,
    workdir: &std::path::Path,
    program: &str,
    network_mode: Option<&str>,
    read_only_worktree: bool,
    container_name: Option<&str>,
    container_env: &[(String, String)],
    limits: Option<&agentgrid_adapters::ResourceLimits>,
) -> (String, Vec<String>) {
    match kind {
        SandboxKind::None => (program.to_string(), vec![]),
        SandboxKind::Docker => {
            let mut prefix = docker_run_head(
                workdir,
                network_mode,
                read_only_worktree,
                container_name,
                container_env,
                limits,
            );
            prefix.push(image_ref());
            prefix.push(program.into());
            ("docker".into(), prefix)
        }
        // Plan 6.11: workdir/env are already the daemon's own — the scope
        // only adds the cgroup boundary, so unlike docker there is no
        // mount/env-forwarding to redo here.
        SandboxKind::Systemd => {
            let unit = container_name
                .and_then(|n| n.strip_prefix("agentgrid-"))
                .map(scope_unit)
                .unwrap_or_else(|| "agentgrid-scope-unknown".to_string());
            let mut prefix = systemd_run_head(&unit, limits);
            prefix.push(program.into());
            ("systemd-run".into(), prefix)
        }
    }
}

/// True when `net` is an `allowlist:` spec and every CIDR in it parses.
/// Egress allowlisting is NOT enforceable via the docker CLI alone (no native
/// per-CIDR egress filter) — see [`net_allowlist_enforceable`].
fn valid_allowlist_spec(net: &str) -> bool {
    let Some(cidrs) = net.strip_prefix("allowlist:") else {
        return false;
    };
    !cidrs.is_empty() && cidrs.split(',').all(|c| parse_cidr(c).is_some())
}

/// The docker-native network a task-level mode resolves to, without the
/// env fallback (used for egress audit logging so operators see the actual
/// isolation applied): `none`→`none`, `restricted`→the configured egress-proxy
/// network when one is set, else `none` (docker cannot do per-range egress
/// filtering), `unrestricted`→`bridge`, anything else passes through. Mirrors
/// the mapping in [`docker_run_head`].
pub fn resolved_network_mode(mode: &str) -> String {
    match mode {
        // Audit X-N3: both the proxy network AND its URL must be configured
        // for a restricted attempt to leave `none` — a bridge with no
        // filtering URL is unfiltered internet, not an upgrade.
        "restricted" => match (egress_proxy_network(), egress_proxy_url()) {
            (Some(net), Some(_)) => net,
            _ => "none".to_string(),
        },
        "unrestricted" => "bridge".to_string(),
        "none" => "none".to_string(),
        _ => "none".to_string(),
    }
}

/// Competitor-gap feature (egress firewall): the operator-configured docker
/// network that hosts an egress proxy sidecar (see `deploy/egress-proxy/`).
/// When set, `network_mode=restricted` attempts attach to this network and get
/// the proxy URL injected (`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`) instead of
/// being collapsed to `--network none` — so "internet but no LAN" becomes
/// enforceable through the proxy's allowlist instead of turning the network
/// fully off.
pub fn egress_proxy_network() -> Option<String> {
    std::env::var("AGENTGRID_SANDBOX_EGRESS_NETWORK")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The egress-proxy URL injected into restricted-mode sandbox containers, if
/// the operator configured one.
pub fn egress_proxy_url() -> Option<String> {
    std::env::var("AGENTGRID_SANDBOX_EGRESS_PROXY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// True when the effective egress is isolated: `none`; a `restricted`
/// attempt collapsed to `none` (no usable proxy); or `restricted` attached
/// to a configured egress-proxy network WITH its filtering URL. A proxy
/// network without a URL is unfiltered internet (fail-open), so it reports
/// NOT isolated (audit X-N2: the old code reported it isolated).
pub fn effective_egress_isolated(mode: &str) -> bool {
    match mode {
        "restricted" => match (egress_proxy_network(), egress_proxy_url()) {
            (None, _) => true,
            (Some(_), Some(_)) => true,
            (Some(_), None) => false,
        },
        other => other == "none",
    }
}

/// Accepts `IP/len` (v4 or v6) and returns the parsed network address + prefix
/// length on success. No new dependency: the handful of checks below cover the
/// forms operators actually type.
fn parse_cidr(s: &str) -> Option<()> {
    let (ip, len) = s.split_once('/')?;
    let len: u8 = len.parse().ok()?;
    if ip.contains(':') {
        (len <= 128 && parse_ipv6(ip)).then_some(())
    } else {
        (len <= 32 && parse_ipv4(ip)).then_some(())
    }
}

fn parse_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| p.parse::<u8>().is_ok() && !(p.len() > 1 && p.starts_with('0')))
}

fn parse_ipv6(s: &str) -> bool {
    // Minimal: must contain a colon and only hex/colon/dot chars.
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
}

/// Build the leading `docker run …` argument vector shared by both spawn
/// paths: housekeeping flags (`--rm -i`), the security hardening flags (plan
/// §25: entrypoint clear, cap-drop, no-new-privileges, network none, optional
/// read-only + tmpfs, optional pids/memory/cpus limits), the worktree mount at
/// `/ag`, and the `--` separator. The caller appends `<image> [program args]`
/// after it.
///
/// Knobs (all optional, env-driven so the sandbox wrapper need not change
/// its call sites to tighten isolation): `AGENTGRID_SANDBOX_NETWORK` (default
/// `none`), `AGENTGRID_SANDBOX_READ_ONLY=1` (read-only root + tmpfs `/tmp`),
/// `AGENTGRID_SANDBOX_PIDS_LIMIT`, `AGENTGRID_SANDBOX_MEMORY`,
/// `AGENTGRID_SANDBOX_CPUS`, `AGENTGRID_SANDBOX_IMAGE_DIGEST` (pins the image
/// by digest when `AGENTGRID_SANDBOX_IMAGE` is a tag).
///
/// Stage 12 / ADR 0003: `limits` is the per-attempt profile ceiling. Each set
/// field overrides the node-wide env knob for this run (per-attempt
/// isolation, not a global hammer); unset fields keep the env fallback so a
/// node operator can still bound every attempt without a profile.
///
/// `--rm` is emitted only for unnamed transient commands (validation/eval
/// probes). Named per-attempt containers are NOT `--rm`-ed: the OOM inspection
/// (`inspect_container_oom`) races auto-removal otherwise, and
/// `remove_sandbox_container` (cancel/timeout/cleanup) plus the startup
/// orphan sweep already reap them.
fn docker_run_head(
    workdir: &std::path::Path,
    network_mode: Option<&str>,
    read_only_worktree: bool,
    container_name: Option<&str>,
    container_env: &[(String, String)],
    limits: Option<&agentgrid_adapters::ResourceLimits>,
) -> Vec<String> {
    let mut v = vec!["run".to_string()];
    if container_name.is_none() {
        v.push("--rm".to_string());
    }
    v.push("-i".to_string());
    // The node always spawns an explicit `<program> <args>`; an image
    // ENTRYPOINT (the GHCR node image ships one: the daemon itself) would
    // swallow the program and run the entrypoint with our args instead.
    // Clear it so the explicit command wins for ANY image.
    v.push("--entrypoint".to_string());
    v.push("".to_string());
    v.push("--cap-drop=ALL".to_string());
    v.push("--security-opt=no-new-privileges".to_string());
    // Audit ND-6: a deterministic per-attempt name so cancel/timeout can
    // `docker rm -f` the actual container. Killing the attached `docker run`
    // client only proxies SIGTERM; the 10s SIGKILL escalation kills the
    // client without forwarding, and the client's death leaves the container
    // running on the worktree mount.
    if let Some(name) = container_name {
        v.push("--name".to_string());
        v.push(name.to_string());
    }
    // Plan §25: stamp the owning daemon on every container so a hard-crashed
    // daemon's orphaned containers are findable/removable at next startup.
    if let Some(node_id) = NODE_ID.get() {
        v.push("--label".to_string());
        v.push(format!("agentgrid.node={node_id}"));
    }
    // Task network_mode overrides env, clamped by node max (enforced at CP).
    let net = network_mode
        .map(|s| s.to_string())
        .or_else(|| std::env::var("AGENTGRID_SANDBOX_NETWORK").ok())
        .unwrap_or_else(|| "none".to_string());
    // Plan §27: translate the task-level mode (none|restricted|unrestricted)
    // into a docker-native network. `restricted` cannot be enforced as
    // "internet but no LAN" with the docker CLI alone (no per-range egress
    // filter); it collapses to `none` — strictly more isolated than promised,
    // never less — UNLESS the operator wired an egress-proxy sidecar
    // (competitor-gap feature): then restricted attempts attach to the proxy
    // network and get HTTP(S)_PROXY injected, and the proxy enforces the
    // allowlist.
    let net = match net.as_str() {
        "none" => "none".to_string(),
        // Audit X-N3: attach to the proxy network only when a filtering URL
        // is configured too — a bridge with no proxy env is unfiltered
        // internet, a downgrade from `none`, not an upgrade.
        "restricted" => {
            if let (Some(egress), Some(_)) = (egress_proxy_network(), egress_proxy_url()) {
                tracing::debug!(
                    network = %egress,
                    "network_mode=restricted attaches to the egress-proxy network"
                );
                egress
            } else {
                tracing::debug!(
                    "network_mode=restricted maps to --network none (no usable \
                     egress proxy configured)"
                );
                "none".to_string()
            }
        }
        "unrestricted" => "bridge".to_string(),
        // Fail closed for TASK-supplied modes only: an unknown string from an
        // assignment payload (e.g. "host") must never reach --network
        // verbatim — the egress audit (resolved_network_mode) already reports
        // unknowns as "none", so executing anything else would grant egress
        // the operator never signed off on. The operator env fallback stays
        // trusted (validated at startup; may name an egress-proxy network).
        other if network_mode.is_some() => {
            tracing::warn!(mode = other, "unknown task network_mode clamped to none");
            "none".to_string()
        }
        other => other.to_string(),
    };
    v.push("--network".to_string());
    v.push(net);
    // Audit X-N3b: forward the attempt env INTO the container. The backend
    // applies SpawnRequest.env only to the `docker` client process, so
    // without these flags credentials/proxies/managed env never reached
    // sandboxed agents. (The proxy-URL block below stays as the restricted
    // path's explicit variant; when it fires the same pairs simply appear
    // twice — harmless, last-wins identical values.)
    for (k, val) in container_env {
        v.push("--env".to_string());
        v.push(format!("{k}={val}"));
    }
    // Competitor-gap feature (egress firewall): inject the proxy URL so
    // restricted-mode adapters route through the sidecar (which enforces the
    // domain allowlist). Only applied when a restricted attempt attached to
    // the egress network — an unrestricted/none attempt must not inherit it.
    if network_mode == Some("restricted") && egress_proxy_network().is_some() {
        if let Some(proxy) = egress_proxy_url() {
            for key in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                v.push("--env".to_string());
                v.push(format!("{key}={proxy}"));
            }
        }
    }
    if std::env::var("AGENTGRID_SANDBOX_READ_ONLY")
        .map(|x| x == "1" || x.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        v.push("--read-only".to_string());
        v.push("--tmpfs".to_string());
        v.push("/tmp".to_string());
    }
    // Stage 12 / ADR 0003: per-attempt limits from the profile override the
    // node-wide env knobs; unset fields fall back to the env default.
    let limits = limits.cloned().unwrap_or_default();
    let pids = limits
        .tasks_max
        .map(|n| n.to_string())
        .or_else(|| env_nonempty("AGENTGRID_SANDBOX_PIDS_LIMIT"));
    if let Some(p) = pids {
        v.push("--pids-limit".to_string());
        v.push(p);
    }
    let mem = limits
        .memory_max
        .map(|b| format!("{}m", (b / (1024 * 1024)).max(1)))
        .or_else(|| env_nonempty("AGENTGRID_SANDBOX_MEMORY"));
    if let Some(m) = mem {
        v.push("--memory".to_string());
        v.push(m);
    }
    let cpus = limits
        .cpu_quota_percent
        .map(|c| {
            // CPUQuota 200% == 2 cores; docker --cpus takes cores directly.
            let whole = c / 100;
            let frac = c % 100;
            if frac == 0 {
                whole.to_string()
            } else {
                format!("{whole}.{frac:02}")
            }
        })
        .or_else(|| env_nonempty("AGENTGRID_SANDBOX_CPUS"));
    if let Some(c) = cpus {
        v.push("--cpus".to_string());
        v.push(c);
    }
    // Plan §25: separate artifact/output mount. When the worktree is
    // read-only the agent still needs a writable place for outputs:
    // `AGENTGRID_SANDBOX_ARTIFACT_DIR=<host dir>` mounts it read-write at
    // `/artifacts` in the container (independent of --read-only).
    if let Ok(d) = std::env::var("AGENTGRID_SANDBOX_ARTIFACT_DIR") {
        if !d.is_empty() {
            v.push("-v".to_string());
            v.push(format!("{d}:/artifacts"));
        }
    }
    v.push("-v".to_string());
    // Plan 2.4 (#22a): workflow verifier steps get a read-only worktree
    // mount so a verifier cannot silently edit the code it is supposed to
    // validate. `:ro` uses the same bind, no second mount needed.
    let suffix = if read_only_worktree { ":ro" } else { "" };
    v.push(format!("{}:/ag{suffix}", workdir.display()));
    v.push("-w".to_string());
    v.push("/ag".to_string());
    v.push("--".to_string());
    v
}

/// The image reference to run, pinned by digest when
/// `AGENTGRID_SANDBOX_IMAGE_DIGEST` is set and the image is a bare tag.
pub(crate) fn image_ref() -> String {
    let image =
        std::env::var("AGENTGRID_SANDBOX_IMAGE").unwrap_or_else(|_| "ubuntu:24.04".to_string());
    if image.contains('@') {
        return image;
    }
    if let Ok(d) = std::env::var("AGENTGRID_SANDBOX_IMAGE_DIGEST") {
        if !d.is_empty() {
            return format!("{image}@{d}");
        }
    }
    image
}

fn env_nonempty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Wrap `(program, args)` for the configured sandbox, rooted at `workdir`.
/// `None` returns the command unchanged. `Docker` prefixes with the hardened
/// `docker run … <image> --` head from [`docker_run_head`].
/// ponytail: binds the whole workdir read-write; a stricter mount policy
/// (read-only + separate artifact dir) is the upgrade path once a real
/// DockerBackend trait owns the worktree/artifact mounts.
// Audit X-N3b: eight assembly knobs is the documented shape of these two
// builders (each has a narrow, distinct role); splitting them into a config
// struct would churn every caller for no gain.
#[allow(clippy::too_many_arguments)]
pub fn sandbox_command(
    kind: SandboxKind,
    program: &str,
    args: &[String],
    workdir: &std::path::Path,
    network_mode: Option<&str>,
    read_only_worktree: bool,
    container_name: Option<&str>,
    container_env: &[(String, String)],
    limits: Option<&agentgrid_adapters::ResourceLimits>,
) -> (String, Vec<String>) {
    match kind {
        SandboxKind::None => (program.to_string(), args.to_vec()),
        SandboxKind::Docker => {
            let mut out = docker_run_head(
                workdir,
                network_mode,
                read_only_worktree,
                container_name,
                container_env,
                limits,
            );
            out.push(image_ref());
            out.push(program.to_string());
            out.extend(args.iter().cloned());
            ("docker".into(), out)
        }
        SandboxKind::Systemd => {
            let unit = container_name
                .and_then(|n| n.strip_prefix("agentgrid-"))
                .map(scope_unit)
                .unwrap_or_else(|| "agentgrid-scope-unknown".to_string());
            let mut out = systemd_run_head(&unit, limits);
            out.push(program.to_string());
            out.extend(args.iter().cloned());
            ("systemd-run".into(), out)
        }
    }
}

/// Hardening P0/P1 (item 5): an unsafe-unattended adapter run (one that
/// bypasses interactive permission prompts / auto-runs every tool call) must
/// NOT happen when the agent is unsandboxed (shares the node environment),
/// unless the operator sets the explicit `AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX=1`
/// override. Returns the env-var names to remove from the adapter subprocess so
/// it falls back to safe mode. Composing the spawn with `cmd.env_remove(name)`
/// for each entry keeps the inherited parent env honest.
///
/// Callers: any path that runs the agent adapter (`ProcessBackend::spawn` and
/// the wrapper-binary spawn) should apply this so the parent's env cannot
/// silently make an unsandboxed run unsafe.
pub fn unsafe_env_guard(kind: SandboxKind) -> Vec<String> {
    if kind != SandboxKind::None {
        return Vec::new();
    }
    let allow = std::env::var("AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if allow {
        return Vec::new();
    }
    let mut remove = vec!["AGENTGRID_UNSAFE_UNATTENDED".to_string()];
    // Per-adapter auto knobs are the other way an operator can opt into a
    // dangerous unattended run; gate them too so the override is the single
    // explicit path.
    if std::env::var("AGENTGRID_OPENCODE_AUTO").is_ok() {
        remove.push("AGENTGRID_OPENCODE_AUTO".to_string());
    }
    tracing::warn!(
        kind = ?kind,
        removed = ?remove,
        "unsafe adapter mode gated off: AGENTGRID_SANDBOX=none and no \
         AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX override; removing the bypass env \
         so the adapter runs in safe mode"
    );
    remove
}

/// Plan §25: fail-closed validation of the network mode at daemon startup.
/// `allowlist:` egress specs are syntactically validated (so a typo'd CIDR
/// fails here, not mid-attempt) but refused — docker has no native egress
/// allowlist, so running would silently deliver full egress. Returns Err for
/// a malformed or unenforceable spec; Ok for `none`/`bridge`/`host`/`internal`.
pub fn validate_network_mode(net: &str) -> anyhow::Result<()> {
    if net.starts_with("allowlist:") {
        if !valid_allowlist_spec(net) {
            anyhow::bail!(
                "AGENTGRID_SANDBOX_NETWORK={net}: malformed allowlist spec (expected                  allowlist:<cidr>[,<cidr>…], e.g. allowlist:10.0.0.0/8,1.2.3.4/32)"
            );
        }
        anyhow::bail!(
            "AGENTGRID_SANDBOX_NETWORK={net}: egress allowlist is not enforceable via the              docker CLI (no native per-CIDR egress filter); refusing to run with a silently              unenforced allowlist. Use none/bridge/host, or wire an egress proxy and pass              its network"
        );
    }
    Ok(())
}

/// Probe the container runtime (plan §25: verify runtime version and
/// capability at startup, not just binary presence). Runs
/// `docker version --format '{{.Server.Version}}'` (podman accepts the same
/// flag) and returns the server version. `Ok(None)` when the runtime binary
/// is missing or the daemon is unreachable — the caller decides whether that
/// is fatal.
pub async fn probe_runtime_version() -> anyhow::Result<Option<String>> {
    let runtime =
        std::env::var("AGENTGRID_SANDBOX_RUNTIME").unwrap_or_else(|_| "docker".to_string());
    let out = tokio::process::Command::new(&runtime)
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .await?;
    if !out.status.success() {
        return Ok(None);
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(if v.is_empty() { None } else { Some(v) })
}

/// Plan §25: verify the adapter binary actually exists inside the sandbox
/// image (the host-side `probe_adapter` proves nothing about the container).
/// Runs `docker run --rm --entrypoint sh <image> -c 'command -v <bin>'` —
/// returns true when the adapter is found. `Err`/false on a missing runtime
/// or image — the caller logs and continues (node reports degraded, scheduler
/// excludes it).
pub async fn probe_adapter_in_sandbox(bin: &str) -> anyhow::Result<bool> {
    let runtime =
        std::env::var("AGENTGRID_SANDBOX_RUNTIME").unwrap_or_else(|_| "docker".to_string());
    // --network none mirrors how the sandbox itself runs the adapter; with a
    // default (bridge) network the probe drags in netavark/nftables setup,
    // which fails under rootless podman on minimal service environments
    // (no user session, WSL2 kernel) — reporting a healthy image as missing.
    let out = tokio::process::Command::new(&runtime)
        .args([
            "run",
            "--rm",
            "--network",
            "none",
            "--entrypoint",
            "sh",
            &image_ref(),
            "-c",
            &format!("command -v {bin}"),
        ])
        .output()
        .await?;
    Ok(out.status.success())
}

/// Plan 6.11: probe the systemd transient-scope capability at startup /
/// heartbeat. A scope backend is usable when (a) `systemd-run` resolves,
/// (b) the process runs under a systemd user manager (`--user` queries
/// answer — a `systemd-run` from a plain SSH session without
/// `loginctl enable-linger` fails here), and (c) the cgroups v2
/// controller is mounted. Returns the systemd version when all three hold,
/// `None` otherwise — the caller reports the scope backend absent
/// (fail-closed: no scope, no scope sandbox).
pub async fn probe_systemd_scope() -> Option<String> {
    // (a) binary present.
    resolve_binary("systemd-run")?;
    // (b) a user manager answers: `systemctl --user is-system-running`
    // succeeds (any "running/degraded" verdict) only with DBus + the user
    // manager up. This is exactly what `systemd-run --user` needs.
    let out = tokio::process::Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // (c) cgroups v2 (unified hierarchy) mounted — the properties
    // MemoryMax/CPUQuota/TasksMax are v2 knobs.
    if !std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        return None;
    }
    let v = tokio::process::Command::new("systemctl")
        .arg("--version")
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1).map(|v| v.to_string()))
        })
        .filter(|s| !s.is_empty());
    Some(v.unwrap_or_else(|| "unknown".to_string()))
}

fn resolve_binary(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(bin);
        if p.is_file() {
            return Some(p);
        }
        #[cfg(windows)]
        {
            let pexe = dir.join(format!("{bin}.exe"));
            if pexe.is_file() {
                return Some(pexe);
            }
        }
    }
    None
}

/// Per-daemon identity stamped as `--label agentgrid.node=<id>` on every
/// sandbox container, so orphan cleanup after a hard daemon crash can find
/// (and remove) exactly this daemon's containers. Set once at startup by
/// `main`; containers spawned before that cannot exist (no sandbox runs
/// before enrollment).
static NODE_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn set_node_id(id: &str) {
    let _ = NODE_ID.set(id.to_string());
}

/// Remove containers this daemon left running after a hard crash (plan §25:
/// `docker run` is attached, so a SIGKILLed daemon can strand a container;
/// `--rm` only fires on clean exits). Kills and removes `agentgrid.node=<id>`
/// containers — best-effort; a missing runtime is not fatal at startup.
pub async fn cleanup_orphan_containers() {
    let Some(node_id) = NODE_ID.get() else {
        return;
    };
    let runtime =
        std::env::var("AGENTGRID_SANDBOX_RUNTIME").unwrap_or_else(|_| "docker".to_string());
    // The filter needs the `label=` prefix — a bare `name=value` is rejected
    // by both docker ("Invalid filter") and podman 5.x ("invalid filter").
    let label = format!("label=agentgrid.node={node_id}");
    let out = match tokio::process::Command::new(&runtime)
        .args(["ps", "-aq", "--filter", &label])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "orphan-container scan failed");
            return;
        }
    };
    if !out.status.success() {
        tracing::warn!(
            "orphan-container scan failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return;
    }
    let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    if ids.is_empty() {
        return;
    }
    let kill = match tokio::process::Command::new(&runtime)
        .args(["rm", "-f"])
        .args(&ids)
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "orphan-container removal failed");
            return;
        }
    };
    if kill.status.success() {
        tracing::info!(count = ids.len(), "removed orphan sandbox containers");
    } else {
        tracing::warn!(
            "orphan-container removal failed: {}",
            String::from_utf8_lossy(&kill.stderr).trim()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Process-global env vars are shared across tests; serialize the
    /// sandbox-env mutators so parallel runs cannot race on
    /// `AGENTGRID_SANDBOX_*` (pre-existing flake: docker_pins_image_by_digest
    /// vs docker_opts_read_only_and_resource_limits).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn none_passthrough() {
        let (p, a) = sandbox_command(
            SandboxKind::None,
            "claude",
            &["--acp".into()],
            std::path::Path::new("/w"),
            None,
            false,
            None,
            &[],
            None,
        );
        assert_eq!(p, "claude");
        assert_eq!(a, vec!["--acp"]);
    }

    #[test]
    fn docker_wraps_command() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
        set_node_id("node-test-633");
        let (p, a) = sandbox_command(
            SandboxKind::Docker,
            "claude",
            &["--acp".into()],
            std::path::Path::new("/w"),
            None,
            false,
            None,
            &[],
            None,
        );
        clear_sandbox_env();
        assert_eq!(p, "docker");
        assert_eq!(a[0], "run");
        assert!(a.contains(&"-v".to_string()));
        // Hardening §25: cap-drop + no-new-privileges always present.
        assert!(a.contains(&"--cap-drop=ALL".to_string()));
        assert!(a.contains(&"--security-opt=no-new-privileges".to_string()));
        // Plan §25: owning-daemon label is stamped when node id is known.
        assert!(a.contains(&"--label".to_string()));
        assert!(a.contains(&"agentgrid.node=node-test-633".to_string()));
        // Default network isolation.
        assert_eq!(
            a[a.iter().position(|x| x == "--network").unwrap() + 1],
            "none"
        );
        // Tail unchanged: <image> <program> <args>.
        assert_eq!(a[a.len() - 3], "img:1");
        assert_eq!(a[a.len() - 2], "claude");
        assert_eq!(a[a.len() - 1], "--acp");
    }

    #[test]
    fn none_prefix_passthrough() {
        // Stage 11.2 / line 358: no sandbox → identity bin, empty prefix.
        let (p, a) = sandbox_prefix(
            SandboxKind::None,
            std::path::Path::new("/w"),
            "adapter-x",
            None,
            false,
            None,
            &[],
            None,
        );
        assert_eq!(p, "adapter-x");
        assert!(a.is_empty());
    }

    #[test]
    fn docker_clears_image_entrypoint() {
        // Lab finding (WSL2 track B, 2026-08-17): an image whose ENTRYPOINT is
        // a binary (the GHCR node image ships the daemon as ENTRYPOINT) swallows
        // the explicit `<program> <args>` — docker/podman runs
        // `<entrypoint> <program> <args>` instead of `<program> <args>`. The
        // node always spawns an explicit command, so it must clear the
        // entrypoint; `--entrypoint ""` makes the explicit command win for any
        // image.
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
        let (_, a) = sandbox_command(
            SandboxKind::Docker,
            "adapter-mock",
            &["--prompt".into(), "hi".into()],
            std::path::Path::new("/w"),
            None,
            false,
            None,
            &[],
            None,
        );
        clear_sandbox_env();
        let idx = a
            .iter()
            .position(|x| x == "--entrypoint")
            .expect("--entrypoint flag missing");
        assert_eq!(
            a[idx + 1],
            "",
            "entrypoint must be cleared so the explicit command wins"
        );
    }

    #[test]
    fn docker_prefix_wraps_program() {
        let _g = ENV_LOCK.lock().unwrap();
        // Legacy wrapper path: program runs inside the image after `--`, with
        // an empty `args` slot (ProcessBackend appends `--prompt` itself).
        clear_sandbox_env();
        std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
        let (p, a) = sandbox_prefix(
            SandboxKind::Docker,
            std::path::Path::new("/w"),
            "adapter-claude",
            None,
            false,
            None,
            &[],
            None,
        );
        clear_sandbox_env();
        assert_eq!(p, "docker");
        assert_eq!(a[0], "run");
        assert!(a.contains(&"-v".to_string()));
        assert_eq!(a[a.len() - 2], "img:1");
        assert_eq!(a[a.len() - 1], "adapter-claude");
    }

    fn clear_sandbox_env() {
        for k in [
            "AGENTGRID_SANDBOX_IMAGE",
            "AGENTGRID_SANDBOX_IMAGE_DIGEST",
            "AGENTGRID_SANDBOX_NETWORK",
            "AGENTGRID_SANDBOX_READ_ONLY",
            "AGENTGRID_SANDBOX_PIDS_LIMIT",
            "AGENTGRID_SANDBOX_MEMORY",
            "AGENTGRID_SANDBOX_CPUS",
            "AGENTGRID_SANDBOX_ARTIFACT_DIR",
            "AGENTGRID_SANDBOX_EGRESS_NETWORK",
            "AGENTGRID_SANDBOX_EGRESS_PROXY",
            "AGENTGRID_SANDBOX",
        ] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn docker_pins_image_by_digest() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
        std::env::set_var("AGENTGRID_SANDBOX_IMAGE_DIGEST", "sha256:deadbeef");
        let (p, a) = sandbox_prefix(
            SandboxKind::Docker,
            std::path::Path::new("/w"),
            "c",
            None,
            false,
            None,
            &[],
            None,
        );
        clear_sandbox_env();
        assert_eq!(p, "docker");
        // image + program are the last two; image must carry the digest pin.
        assert_eq!(a[a.len() - 2], "img:1@sha256:deadbeef");
        assert_eq!(a[a.len() - 1], "c");
        // An already-digested ref is left untouched.
        std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1@sha256:f00d");
        let (_, a2) = sandbox_prefix(
            SandboxKind::Docker,
            std::path::Path::new("/w"),
            "c",
            None,
            false,
            None,
            &[],
            None,
        );
        clear_sandbox_env();
        assert_eq!(a2[a2.len() - 2], "img:1@sha256:f00d");
    }

    #[test]
    fn read_only_worktree_marks_mount_ro() {
        // Plan 2.4 (#22a): when the assignment is flagged read-only (verifier
        // step), the worktree bind-mount gets a `:ro` suffix so the agent in
        // the sandbox cannot modify the code it is supposed to validate.
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
        let (_p, a) = sandbox_command(
            SandboxKind::Docker,
            "c",
            &[],
            std::path::Path::new("/work"),
            None,
            true,
            None,
            &[],
            None,
        );
        clear_sandbox_env();
        let vol = a
            .iter()
            .find(|x| x.starts_with("/work:/ag"))
            .expect("worktree mount missing");
        assert!(
            vol.ends_with(":ro"),
            "worktree mount must be :ro, got {vol}"
        );
        // And the default (non-verifier) path stays read-write.
        let (_p2, a2) = sandbox_command(
            SandboxKind::Docker,
            "c",
            &[],
            std::path::Path::new("/work"),
            None,
            false,
            None,
            &[],
            None,
        );
        let vol2 = a2
            .iter()
            .find(|x| x.starts_with("/work:/ag"))
            .expect("worktree mount missing");
        assert!(vol2.ends_with(":/ag"), "default mount stays rw, got {vol2}");
    }

    /// Plan 2.4 follow-up: prove the `read_only_worktree=true` mount option
    /// actually blocks a write at the kernel — not just that the docker args
    /// contain ":ro" (a stripped path or a wrong mount target would still pass
    /// the substring assertion but leave the agent able to write). Requires a
    /// reachable docker daemon, an image, and `AG_RUST_TEST_DOCKER=1` to opt
    /// in (set in the e2e CI job where docker is available); otherwise it
    /// no-ops so `cargo test` on a docker-less runner stays green.
    ///
    /// opt-in gate (not `#[ignore]`) keeps it runnable from `cargo test` on a
    /// dev box without tweaking the harness.
    #[test]
    fn docker_ro_mount_really_blocks_write() {
        if std::env::var("AG_RUST_TEST_DOCKER").ok().as_deref() != Some("1") {
            return;
        }
        let image =
            std::env::var("AGENTGRID_SANDBOX_IMAGE").unwrap_or_else(|_| "ubuntu:24.04".into());
        // Skip when the image or the daemon is not available — keeps `cargo
        // test` green on a docker-less or image-less runner (e.g. the fast
        // `rust` CI job) without masking the test where it SHOULD run (the
        // e2e job, which builds the image and sets the opt-in env).
        if std::process::Command::new("docker")
            .args(["image", "inspect", &image])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            return;
        }

        // Create a scratch host dir to mount read-only into the container.
        let host = std::env::temp_dir().join(format!("ag-ro-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join(".marker"), b"").unwrap();

        // `test ! -w /ag` succeeds (exit 0) only when the mount is actually RO.
        // `--entrypoint sh` overrides an image that ships its own entrypoint
        // (e.g. the agentgrid node image) so the "/ag" write-test runs.
        let out = std::process::Command::new("docker")
            .args([
                "run",
                "--rm",
                "--read-only",
                "--entrypoint",
                "sh",
                "-v",
                &format!("{}:/ag:ro", host.display()),
                &image,
                "-c",
                "test ! -w /ag && ! touch /ag/evil 2>/dev/null",
            ])
            .output();
        let _ = std::fs::remove_dir_all(&host);
        match out {
            Ok(o) => assert!(
                o.status.success(),
                "read-only mount let the verifier write: status={:?} stderr={}",
                o.status.code(),
                String::from_utf8_lossy(&o.stderr),
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skip docker_ro_mount_really_blocks_write: docker not found");
            }
            Err(e) => panic!("failed to spawn docker: {e}"),
        }
    }

    #[test]
    fn docker_opts_read_only_and_resource_limits() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
        std::env::set_var("AGENTGRID_SANDBOX_NETWORK", "bridge");
        std::env::set_var("AGENTGRID_SANDBOX_READ_ONLY", "1");
        std::env::set_var("AGENTGRID_SANDBOX_PIDS_LIMIT", "128");
        std::env::set_var("AGENTGRID_SANDBOX_MEMORY", "512m");
        std::env::set_var("AGENTGRID_SANDBOX_CPUS", "1.5");
        let (_, a) = sandbox_command(
            SandboxKind::Docker,
            "c",
            &[],
            std::path::Path::new("/w"),
            None,
            false,
            None,
            &[],
            None,
        );
        clear_sandbox_env();
        let at = |flag: &str| a.iter().position(|x| x == flag).unwrap() + 1;
        assert_eq!(a[at("--network")], "bridge");
        assert!(a.contains(&"--read-only".to_string()));
        assert_eq!(a[at("--tmpfs")], "/tmp");
        assert_eq!(a[at("--pids-limit")], "128");
        assert_eq!(a[at("--memory")], "512m");
        assert_eq!(a[at("--cpus")], "1.5");
    }

    #[test]
    fn task_network_mode_maps_to_docker_native_networks() {
        let _g = ENV_LOCK.lock().unwrap();
        let net_of = |mode: Option<&str>| {
            clear_sandbox_env();
            let (_, a) = sandbox_command(
                SandboxKind::Docker,
                "c",
                &[],
                std::path::Path::new("/w"),
                mode,
                false,
                None,
                &[],
                None,
            );
            let i = a.iter().position(|x| x == "--network").unwrap() + 1;
            a[i].clone()
        };
        // none -> none, unrestricted -> bridge, restricted -> none (safe
        // ceiling: docker cannot do "internet but no LAN" natively).
        assert_eq!(net_of(Some("none")), "none");
        assert_eq!(net_of(Some("unrestricted")), "bridge");
        assert_eq!(net_of(Some("restricted")), "none");
        assert_eq!(net_of(None), "none", "default remains none");
        // Unknown TASK-supplied modes fail closed: a CP-controlled "host"
        // (or any garbage) must never reach --network verbatim.
        assert_eq!(net_of(Some("host")), "none");
        assert_eq!(net_of(Some("made-up")), "none");
    }

    #[test]
    fn restricted_mode_uses_egress_proxy_network_when_configured() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        std::env::set_var("AGENTGRID_SANDBOX_EGRESS_NETWORK", "agentgrid-egress");
        std::env::set_var("AGENTGRID_SANDBOX_EGRESS_PROXY", "http://egress:3128");
        let (_, a) = sandbox_command(
            SandboxKind::Docker,
            "c",
            &[],
            std::path::Path::new("/w"),
            Some("restricted"),
            false,
            None,
            &[],
            None,
        );
        clear_sandbox_env();
        let at = |flag: &str| a.iter().position(|x| x == flag).unwrap() + 1;
        assert_eq!(
            a[at("--network")],
            "agentgrid-egress",
            "restricted attaches to the egress-proxy network"
        );
        // Proxy env is injected for restricted attempts on the egress network.
        let envs: Vec<&String> = a.iter().filter(|x| x.starts_with("HTTP_PROXY=")).collect();
        assert_eq!(envs, vec![&"HTTP_PROXY=http://egress:3128".to_string()]);
        let envs: Vec<&String> = a.iter().filter(|x| x.starts_with("HTTPS_PROXY=")).collect();
        assert_eq!(envs, vec![&"HTTPS_PROXY=http://egress:3128".to_string()]);

        // An unrestricted attempt on the same node must NOT inherit the proxy.
        let (_, a2) = sandbox_command(
            SandboxKind::Docker,
            "c",
            &[],
            std::path::Path::new("/w"),
            Some("unrestricted"),
            false,
            None,
            &[],
            None,
        );
        let at2 = |flag: &str| a2.iter().position(|x| x == flag).unwrap() + 1;
        assert_eq!(a2[at2("--network")], "bridge");
        assert!(
            !a2.iter().any(|x| x.starts_with("HTTP_PROXY=")),
            "unrestricted attempt must not inherit the egress proxy env"
        );
    }

    #[test]
    fn allowlist_spec_validation_accepts_cidrs_and_rejects_garbage() {
        assert!(valid_allowlist_spec("allowlist:10.0.0.0/8"));
        assert!(valid_allowlist_spec("allowlist:10.0.0.0/8,1.2.3.4/32"));
        assert!(valid_allowlist_spec("allowlist:2001:db8::/32"));
        assert!(!valid_allowlist_spec("allowlist:"), "empty allowlist");
        assert!(
            !valid_allowlist_spec("allowlist:10.0.0.0/99"),
            "v4 len > 32"
        );
        assert!(!valid_allowlist_spec("allowlist:not-an-ip/8"));
        assert!(
            !valid_allowlist_spec("bridge"),
            "non-allowlist is not a spec"
        );
    }

    #[test]
    fn allowlist_fails_closed_at_startup() {
        // Enforceable modes pass.
        for ok in ["none", "bridge", "host", "internal"] {
            assert!(validate_network_mode(ok).is_ok(), "{ok} must pass");
        }
        // Malformed allowlist -> Err (typo'd CIDR caught before any attempt).
        assert!(validate_network_mode("allowlist:1.2.3.4/99").is_err());
        // Well-formed but unenforceable -> Err (never silently full egress).
        let e = validate_network_mode("allowlist:10.0.0.0/8").unwrap_err();
        let msg = format!("{e:#}");
        assert!(
            msg.contains("not enforceable") || msg.contains("refusing"),
            "error must explain the refusal: {msg}"
        );
    }

    #[test]
    fn docker_artifact_mount_is_read_write_and_independent_of_read_only() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
        std::env::set_var("AGENTGRID_SANDBOX_ARTIFACT_DIR", "/host/art");
        std::env::set_var("AGENTGRID_SANDBOX_READ_ONLY", "1");
        let (_, a) = sandbox_command(
            SandboxKind::Docker,
            "c",
            &[],
            std::path::Path::new("/w"),
            None,
            false,
            None,
            &[],
            None,
        );
        clear_sandbox_env();
        assert!(
            a.contains(&"/host/art:/artifacts".to_string()),
            "artifact dir must be mounted read-write at /artifacts: {a:?}"
        );
        assert!(a.contains(&"--read-only".to_string()));
        // The artifact mount must not carry :ro even under --read-only.
        let art_idx = a.iter().position(|x| x == "/host/art:/artifacts").unwrap();
        assert!(
            !a[art_idx].ends_with(":ro"),
            "artifact mount stays read-write under --read-only"
        );
    }

    #[test]
    fn unsafe_guard_strips_unset_env_when_unsandboxed() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX");
        let remove = unsafe_env_guard(SandboxKind::None);
        assert!(
            remove.contains(&"AGENTGRID_UNSAFE_UNATTENDED".to_string()),
            "unsafe unattended env must be stripped when unsandboxed"
        );
    }

    #[test]
    fn unsafe_guard_keeps_env_when_sandboxed() {
        let remove = unsafe_env_guard(SandboxKind::Docker);
        assert!(remove.is_empty(), "sandboxed runs may keep the unsafe env");
    }

    #[test]
    fn unsafe_guard_keeps_env_with_override() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX", "1");
        let remove = unsafe_env_guard(SandboxKind::None);
        assert!(remove.is_empty(), "explicit override keeps the unsafe env");
        std::env::remove_var("AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX");
    }

    // ---- Stage 12 / ADR 0003: per-attempt resource limits ----

    use agentgrid_adapters::ResourceLimits;

    #[test]
    fn per_attempt_limits_override_env_knobs() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
        // Node-wide env knob says 512m, but the attempt profile says 64 MiB
        // with 0.5 cores and 32 tasks — the per-attempt ceiling must win.
        std::env::set_var("AGENTGRID_SANDBOX_MEMORY", "512m");
        let limits = ResourceLimits {
            memory_max: Some(64 * 1024 * 1024),
            cpu_quota_percent: Some(50),
            tasks_max: Some(32),
        };
        let (_, a) = sandbox_command(
            SandboxKind::Docker,
            "c",
            &[],
            std::path::Path::new("/w"),
            None,
            false,
            None,
            &[],
            Some(&limits),
        );
        clear_sandbox_env();
        let at = |flag: &str| a.iter().position(|x| x == flag).unwrap() + 1;
        assert_eq!(
            a[at("--memory")],
            "64m",
            "profile memory overrides env 512m"
        );
        assert_eq!(
            a[at("--cpus")],
            "0.50",
            "CPUQuota 50% renders as 0.50 cores"
        );
        assert_eq!(a[at("--pids-limit")], "32");
    }

    #[test]
    fn per_attempt_unset_fields_fall_back_to_env() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
        std::env::set_var("AGENTGRID_SANDBOX_PIDS_LIMIT", "100");
        std::env::set_var("AGENTGRID_SANDBOX_CPUS", "2");
        // Only memory is set on the attempt; pids/cpus keep the env value.
        let limits = ResourceLimits {
            memory_max: Some(32 * 1024 * 1024),
            cpu_quota_percent: None,
            tasks_max: None,
        };
        let (_, a) = sandbox_command(
            SandboxKind::Docker,
            "c",
            &[],
            std::path::Path::new("/w"),
            None,
            false,
            None,
            &[],
            Some(&limits),
        );
        clear_sandbox_env();
        let at = |flag: &str| a.iter().position(|x| x == flag).unwrap() + 1;
        assert_eq!(a[at("--memory")], "32m");
        assert_eq!(
            a[at("--pids-limit")],
            "100",
            "unset profile field keeps env"
        );
        assert_eq!(a[at("--cpus")], "2", "unset profile field keeps env");
    }

    #[test]
    fn cpu_quota_renders_whole_cores_without_fraction() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
        for (quota, expect) in [(100, "1"), (200, "2"), (150, "1.50"), (75, "0.75")] {
            let limits = ResourceLimits {
                memory_max: None,
                cpu_quota_percent: Some(quota),
                tasks_max: None,
            };
            let (_, a) = sandbox_command(
                SandboxKind::Docker,
                "c",
                &[],
                std::path::Path::new("/w"),
                None,
                false,
                None,
                &[],
                Some(&limits),
            );
            let at = a.iter().position(|x| x == "--cpus").unwrap() + 1;
            assert_eq!(a[at], expect, "quota {quota}% must render as {expect}");
        }
        clear_sandbox_env();
    }

    #[test]
    fn named_containers_are_not_rm_ed_transient_ones_are() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
        // A named per-attempt container keeps its inspectable state so the
        // OOM check (inspect_container_oom) does not race auto-removal.
        let (_, named) = sandbox_command(
            SandboxKind::Docker,
            "c",
            &[],
            std::path::Path::new("/w"),
            None,
            false,
            Some("agentgrid-attempt-x"),
            &[],
            None,
        );
        let (_, transient) = sandbox_command(
            SandboxKind::Docker,
            "c",
            &[],
            std::path::Path::new("/w"),
            None,
            false,
            None,
            &[],
            None,
        );
        clear_sandbox_env();
        assert!(
            !named.contains(&"--rm".to_string()),
            "named per-attempt container must not be --rm (OOM inspect races it): {named:?}"
        );
        assert!(
            transient.contains(&"--rm".to_string()),
            "transient unnamed probe keeps --rm: {transient:?}"
        );
    }

    // ---- Plan 6.11: systemd transient scope backend ----

    #[test]
    fn scope_unit_sanitizes_attempt_ids() {
        assert_eq!(scope_unit("abc123"), "agentgrid-scope-abc123");
        // Dots are legal in unit names and stay; slashes/spaces (any future
        // id shape) collapse to dashes so an invalid unit name never reaches
        // systemctl.
        assert_eq!(scope_unit("a.b/c d"), "agentgrid-scope-a.b-c-d");
        assert_eq!(scope_unit("weird:chars!"), "agentgrid-scope-weird-chars-");
    }

    #[test]
    fn systemd_prefix_wraps_program_with_scope() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        let (p, a) = sandbox_prefix(
            SandboxKind::Systemd,
            std::path::Path::new("/w"),
            "adapter-mock",
            None,
            false,
            Some("agentgrid-att-42"),
            &[],
            None,
        );
        clear_sandbox_env();
        assert_eq!(p, "systemd-run");
        assert_eq!(a[0], "run");
        assert_eq!(a[1], "--user");
        assert_eq!(a[2], "--scope");
        let unit = a.iter().position(|x| x == "--unit").unwrap() + 1;
        assert_eq!(a[unit], "agentgrid-scope-att-42");
        // Tail unchanged: <program>.
        assert_eq!(a[a.len() - 1], "adapter-mock");
        // No docker-only flags leak into the scope head.
        assert!(!a.iter().any(|x| x == "--cap-drop=ALL"));
        assert!(!a.iter().any(|x| x == "--"));
    }

    #[test]
    fn systemd_maps_limits_to_cgroup_properties() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        let limits = ResourceLimits {
            memory_max: Some(64 * 1024 * 1024),
            cpu_quota_percent: Some(150),
            tasks_max: Some(32),
        };
        let (_, a) = sandbox_prefix(
            SandboxKind::Systemd,
            std::path::Path::new("/w"),
            "adapter-mock",
            None,
            false,
            Some("agentgrid-att-42"),
            &[],
            Some(&limits),
        );
        clear_sandbox_env();
        assert!(a.contains(&"--property=MemoryMax=64M".to_string()));
        assert!(a.contains(&"--property=CPUQuota=150%".to_string()));
        assert!(a.contains(&"--property=TasksMax=32".to_string()));
    }

    #[test]
    fn systemd_unset_limits_fall_back_to_env_knobs() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        std::env::set_var("AGENTGRID_SANDBOX_MEMORY", "256M");
        std::env::set_var("AGENTGRID_SANDBOX_CPUS", "1.5");
        std::env::set_var("AGENTGRID_SANDBOX_PIDS_LIMIT", "64");
        let (_, a) = sandbox_prefix(
            SandboxKind::Systemd,
            std::path::Path::new("/w"),
            "adapter-mock",
            None,
            false,
            Some("agentgrid-att-42"),
            &[],
            None,
        );
        clear_sandbox_env();
        assert!(a.contains(&"--property=MemoryMax=256M".to_string()));
        // Cores knob ("1.5") is normalized into the same %-of-core unit the
        // profile field uses, so one convention covers both sources.
        assert!(a.contains(&"--property=CPUQuota=150%".to_string()));
        assert!(a.contains(&"--property=TasksMax=64".to_string()));
    }

    #[test]
    fn systemd_command_extends_args_after_head() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_sandbox_env();
        let (p, a) = sandbox_command(
            SandboxKind::Systemd,
            "claude",
            &["--acp".into()],
            std::path::Path::new("/w"),
            None,
            false,
            Some("agentgrid-att-7"),
            &[],
            None,
        );
        clear_sandbox_env();
        assert_eq!(p, "systemd-run");
        assert_eq!(a[a.len() - 2], "claude");
        assert_eq!(a[a.len() - 1], "--acp");
        let unit = a.iter().position(|x| x == "--unit").unwrap() + 1;
        assert_eq!(a[unit], "agentgrid-scope-att-7");
    }

    #[test]
    fn sandbox_kind_parses_systemd_aliases() {
        let _g = ENV_LOCK.lock().unwrap();
        for v in [
            "systemd",
            "SYSTEMD",
            " systemd ",
            "systemd-scope",
            "cgroups",
        ] {
            std::env::set_var("AGENTGRID_SANDBOX", v);
            assert_eq!(SandboxKind::from_env(), SandboxKind::Systemd, "{v}");
        }
        std::env::remove_var("AGENTGRID_SANDBOX");
        assert_eq!(SandboxKind::from_env(), SandboxKind::None);
    }

    #[test]
    fn sandbox_kind_systemd_still_routes_to_none_on_garbage() {
        let _g = ENV_LOCK.lock().unwrap();
        for v in ["", "none", "jail", "kvm"] {
            std::env::set_var("AGENTGRID_SANDBOX", v);
            assert_eq!(
                SandboxKind::from_env(),
                SandboxKind::None,
                "'{v}' must never select an isolation backend"
            );
        }
        std::env::remove_var("AGENTGRID_SANDBOX");
    }
}
