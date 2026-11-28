//! Agent isolation (idea: sandcastle-style sandbox abstraction).
//!
//! A `Sandbox` wraps the command the node would run the agent as, so an agent
//! can be confined to a container (Docker/Podman/microVM), a systemd
//! transient scope (cgroups v2) or an unprivileged user namespace
//! (bubblewrap, ADR 0022) instead of sharing the node's full environment. The
//! default `NoSandbox` runs the agent directly in the worktree (legacy
//! behavior). Configured via `AGENTGRID_SANDBOX`
//! (`none` | `docker` | `systemd` | `bwrap`) and `AGENTGRID_SANDBOX_IMAGE`.
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
//!
//! Plan 6.11: separate Unix user for the attempt subprocess via
//! `AGENTGRID_RUN_AS_USER=<user>[:<group>]` (or `<uid>[:<gid>]`). Docker
//! enforces it as `docker run --user` (no privilege needed); the native and
//! systemd-scope paths wrap the command in `runuser -u … -- …`, which needs
//! root — a non-root daemon with the var set refuses to start (fail-closed)
//! instead of silently running the agent as the daemon user. The daemon user
//! keeps owning the worktree mount, so the operator must grant the run-as
//! user write access there (e.g. `useradd -r -m agentgrid-worker` +
//! `setfacl -R -m u:agentgrid-worker:rwx <workspace-root>`).

/// Node-side path to the CA that signed the credential gateway's certificate
/// (ADR 0029 C3). Set only when the gateway is served over TLS.
pub const GATEWAY_CA_ENV: &str = "AGENTGRID_CRED_GATEWAY_CA";

/// Where that CA appears inside the sandbox. One fixed path, because the trust
/// variables below all have to agree on it.
pub const GATEWAY_CA_IN_CONTAINER: &str = "/etc/agentgrid/gateway-ca.pem";

mod bwrap;
mod docker;
mod systemd;

pub use bwrap::*;
pub use docker::*;
pub use systemd::*;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxKind {
    None,
    Docker,
    /// Plan 6.11: run the attempt inside a systemd transient scope
    /// (`systemd-run --user --scope`) so the whole agent tree lands in one
    /// cgroup and the kernel enforces MemoryMax / CPUQuota / TasksMax. No
    /// container runtime required — the native Tier-1 isolation backend.
    Systemd,
    /// Unprivileged namespace confinement via bubblewrap (ADR 0022).
    ///
    /// The two backends above have a hard prerequisite: docker needs a
    /// container runtime, and a systemd scope needs a user manager plus root
    /// for `runuser`. A node with neither — a plain VPS, a CI runner, Termux —
    /// used to fall back to `None` and run the agent as a direct child of the
    /// daemon with the host's filesystem, pid space and network. bwrap needs
    /// only unprivileged user namespaces (default on Debian 12), so it gives
    /// those hosts a mount/pid/net/ipc boundary without a runtime and without
    /// privilege.
    Bwrap,
}

impl SandboxKind {
    /// Parse an `AGENTGRID_SANDBOX` value.
    ///
    /// `Err` for a value that is set but not recognised. The old behaviour —
    /// anything unknown means `None` — turned a typo (`AGENTGRID_SANDBOX=dokcer`)
    /// into "run the agent directly on the host", which is the one outcome this
    /// setting exists to prevent (finding 16).
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        let value = raw.unwrap_or_default().trim().to_ascii_lowercase();
        match value.as_str() {
            "" | "none" | "off" => Ok(SandboxKind::None),
            "docker" | "podman" => Ok(SandboxKind::Docker),
            "systemd" | "systemd-scope" | "cgroups" => Ok(SandboxKind::Systemd),
            "bwrap" | "bubblewrap" | "userns" => Ok(SandboxKind::Bwrap),
            other => Err(format!(
                "AGENTGRID_SANDBOX={other:?} is not a known sandbox: \
                 use none, docker (or podman), systemd, or bwrap"
            )),
        }
    }

    pub fn from_env() -> Self {
        match Self::parse(std::env::var("AGENTGRID_SANDBOX").ok().as_deref()) {
            Ok(kind) => kind,
            // Startup refuses (see `check_startup_config`), so this fallback is
            // unreachable in a running daemon; it only keeps call sites total.
            Err(e) => {
                tracing::error!("{e}");
                SandboxKind::None
            }
        }
    }

    /// True when the sandboxed child is a container/scope that starts from a
    /// different environment and therefore needs the daemon's env forwarded
    /// explicitly. `None` and `Bwrap` children run on the host and inherit the
    /// spawn environment, so callers that build a clean env themselves
    /// (`ProcessBackend::spawn`, the eval probe) must do so for them too.
    pub fn inherits_daemon_env(self) -> bool {
        matches!(self, SandboxKind::Docker | SandboxKind::Systemd)
    }
}

/// Startup gate for the sandbox setting (finding 16).
///
/// An unknown value is always an error. No sandbox at all is allowed in a dev
/// host and refused in `AGENTGRID_ENV=production`, unless unsafe unattended
/// mode was explicitly acknowledged — that mode disables the sandbox by design,
/// and its own gate already ran.
pub fn check_startup_config(unsafe_acknowledged: bool) -> anyhow::Result<()> {
    let raw = std::env::var("AGENTGRID_SANDBOX").ok();
    let kind = SandboxKind::parse(raw.as_deref()).map_err(|e| anyhow::anyhow!(e))?;
    if kind == SandboxKind::None && !unsafe_acknowledged {
        // Boot validates AGENTGRID_ENV; the fail-closed accessor keeps this
        // gate strict even if a caller ever skips that validation.
        let production = agentgrid_common::deploy_env_fail_closed().is_production();
        if production {
            anyhow::bail!(
                "no sandbox is configured (AGENTGRID_SANDBOX is unset or 'none') with \
                 AGENTGRID_ENV=production: attempts would run directly on the host; \
                 set docker, systemd or bwrap"
            );
        }
        tracing::warn!(
            "AGENTGRID_SANDBOX is unset: attempts run directly on the host with no \
             isolation — set docker, systemd or bwrap"
        );
    }
    // Without an explicit spec the container now runs as the worktree owner
    // (see `worktree_user`), never as an arbitrary image user. If that owner
    // is root the agent is still root — say so instead of claiming isolation.
    if kind == SandboxKind::Docker && run_as_spec().is_none() {
        tracing::warn!(
            "docker sandbox runs attempts as the worktree owner (root if the worktree \
             is root-owned); set AGENTGRID_RUN_AS_USER=<uid[:gid]> and chown the \
             worktree to isolate the agent from the daemon user"
        );
    }
    // Finding 3: `restricted` under bwrap is not a restriction. bwrap has no
    // per-CIDR filter and no proxy-only network — `--share-net` plus an
    // `HTTP(S)_PROXY` variable is a hint any process can ignore — so a node
    // configured for it would advertise an egress policy it cannot apply.
    // Refused rather than clamped: the operator asked for a guarantee, and a
    // silent downgrade is the failure this whole finding is about.
    if kind == SandboxKind::Bwrap
        && std::env::var("AGENTGRID_SANDBOX_NETWORK").as_deref() == Ok("restricted")
    {
        anyhow::bail!(
            "AGENTGRID_SANDBOX=bwrap cannot enforce AGENTGRID_SANDBOX_NETWORK=restricted: \
             bwrap has only --unshare-net or the host network, so the proxy would be an \
             environment hint that any process can ignore. Use AGENTGRID_SANDBOX=docker \
             (restricted egress needs its network + proxy), or set \
             AGENTGRID_SANDBOX_NETWORK=none and run attempts with no egress"
        );
    }
    Ok(())
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
        // Plan 6.11: a configured run-as user wraps the raw command in
        // `runuser -u … -- …` (needs root; a non-root daemon refuses to
        // start — see the startup gate in `main`).
        SandboxKind::None => {
            let mut prefix = valid_run_as_prefix();
            prefix.push(program.to_string());
            if prefix.len() == 1 {
                (program.to_string(), vec![])
            } else {
                let bin = prefix.remove(0);
                (bin, prefix)
            }
        }
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
        // mount/env-forwarding to redo here. A configured run-as user wraps
        // the program in `runuser` INSIDE the scope (cgroup still applies).
        SandboxKind::Systemd => {
            let unit = container_name
                .and_then(|n| n.strip_prefix("agentgrid-"))
                .map(scope_unit)
                .unwrap_or_else(|| "agentgrid-scope-unknown".to_string());
            let mut prefix = systemd_run_head(&unit, limits);
            prefix.extend(valid_run_as_prefix());
            prefix.push(program.into());
            ("systemd-run".into(), prefix)
        }
        // ADR 0022: bwrap already runs on the host, so `container_env` is
        // inherited through the spawn environment (the backend hands it a
        // clean env); only the egress-proxy URL is passed explicitly.
        SandboxKind::Bwrap => {
            let masks = bwrap_masked_paths(workdir);
            let mut prefix = bwrap_head(workdir, network_mode, read_only_worktree, &masks);
            // The child runs with a curated environment, so a bare program
            // name has no PATH to resolve against *inside* the namespace:
            // `bwrap: execvp adapter-mock: No such file or directory`. Resolve
            // it on the host, where PATH is the daemon's, and hand bwrap the
            // path. Found on a real Debian node — the unit tests build the
            // argument vector and never execute it.
            let program = crate::capabilities::resolve_in_path(program)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| program.to_string());
            bwrap_rebind_if_masked(&mut prefix, &program, &masks);
            // The masks hid `$HOME` (and the data dir); put back the `PATH`
            // entries that lived under them, or the adapter's own toolchain
            // disappears with the secrets.
            bwrap_rebind_path_dirs(&mut prefix, &masks, std::env::var_os("PATH").as_deref());
            prefix.push(program);
            ("bwrap".into(), prefix)
        }
    }
}

/// Re-expose `program` when the sandbox's own mounts would hide it.
///
/// `bwrap_head` replaces `/tmp` with a tmpfs and masks the host's private
/// directories ([`bwrap_masked_paths`]), so an adapter that lives under one of
/// them exists on the host and is invisible inside the namespace — the symptom
/// is `execvp /tmp/…/adapter-mock: No such file or directory` for a file that
/// is right there. A later bind wins over the tmpfs, so binding the binary back
/// (read-only) is enough, and it costs nothing when the adapter sits in a
/// normal install prefix.
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

/// The proxy variables a `restricted` attempt needs, as `(key, value)` pairs.
///
/// They are exported to the docker *client* process, never written into argv:
/// a proxy URL commonly carries `user:pass@`, and `ps auxww` /
/// `/proc/<pid>/cmdline` are readable by every local user for the whole
/// attempt. `--env KEY` (no value) makes docker copy the value from its own
/// environment, which is where the backend puts these.
pub fn egress_proxy_env() -> Vec<(String, String)> {
    let Some(url) = egress_proxy_url() else {
        return Vec::new();
    };
    [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ]
    .iter()
    .map(|k| ((*k).to_string(), url.clone()))
    .collect()
}

/// True when the effective egress is isolated **for this sandbox kind**.
///
/// Docker: `none` (its own network namespace with no route), or `restricted`
/// collapsed to `none` (no usable proxy), or `restricted` attached to a
/// configured egress-proxy network WITH its filtering URL. A proxy network
/// without a URL is unfiltered internet (fail-open), so it reports NOT
/// isolated (audit X-N2: the old code reported it isolated).
///
/// bwrap: only `none` — it has `--unshare-net` and nothing in between, so a
/// `restricted` attempt keeps the host namespace and its proxy is an
/// environment-variable hint. The answer used to ignore the kind, which made
/// the same mode read as isolated on a bwrap node and as enforced on a docker
/// one.
///
/// systemd and no-sandbox: never. A cgroup scope and a bare process share the
/// host network namespace; there is nothing to unshare.
pub fn effective_egress_isolated(kind: SandboxKind, mode: &str) -> bool {
    match kind {
        SandboxKind::Docker => match mode {
            "restricted" => match (egress_proxy_network(), egress_proxy_url()) {
                (None, _) => true,
                (Some(_), Some(_)) => true,
                (Some(_), None) => false,
            },
            other => other == "none",
        },
        SandboxKind::Bwrap => mode == "none",
        SandboxKind::Systemd | SandboxKind::None => false,
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

/// Numeric `<uid>:<gid>` of the bind-mounted worktree, used as the default
/// `--user` when the operator has not set `AGENTGRID_RUN_AS_USER`. A fixed
/// non-root uid would fail on permissions when the worktree is owned by the
/// daemon; the owner always has write access.
#[cfg(unix)]
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
        // Plan 6.11: same `runuser` wrap as `sandbox_prefix` for the raw
        // command (needs root; non-root daemons fail fast at startup).
        SandboxKind::None => {
            let mut out = valid_run_as_prefix();
            out.push(program.to_string());
            out.extend(args.iter().cloned());
            if out.len() == args.len() + 1 {
                (program.to_string(), args.to_vec())
            } else {
                let bin = out.remove(0);
                (bin, out)
            }
        }
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
            out.extend(valid_run_as_prefix());
            out.push(program.to_string());
            out.extend(args.iter().cloned());
            ("systemd-run".into(), out)
        }
        // ADR 0022: same builder as `sandbox_prefix`; the args follow the
        // `--` terminator so a program name can never be read as a bwrap flag.
        SandboxKind::Bwrap => {
            let masks = bwrap_masked_paths(workdir);
            let mut out = bwrap_head(workdir, network_mode, read_only_worktree, &masks);
            // Same reason as `sandbox_command`: the child's PATH is the
            // curated one, so a bare name does not resolve inside the
            // namespace. Resolve it here, where the daemon's PATH applies.
            let program = crate::capabilities::resolve_in_path(program)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| program.to_string());
            bwrap_rebind_if_masked(&mut out, &program, &masks);
            // Same mounts as `sandbox_prefix`, or the two would disagree on the
            // argv and the validation command would lose the toolchain the
            // agent's shell kept.
            bwrap_rebind_path_dirs(&mut out, &masks, std::env::var_os("PATH").as_deref());
            out.push(program);
            out.extend(args.iter().cloned());
            ("bwrap".into(), out)
        }
    }
}

/// Plan 6.11: the configured separate-Unix-user spec
/// (`AGENTGRID_RUN_AS_USER`), trimmed. Empty/unset → `None` (the attempt
/// subprocess keeps the daemon's uid — the pre-existing behaviour).
pub fn run_as_spec() -> Option<String> {
    match std::env::var("AGENTGRID_RUN_AS_USER") {
        Ok(s) => {
            let t = s.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        }
        Err(_) => None,
    }
}

/// Plan 6.11: fail-closed validation of the run-as spec at daemon startup
/// (mirrors [`validate_network_mode`]). Accepts `<user>[:<group>]` or
/// `<uid>[:<gid>]`: each part is 1–32 chars of `[A-Za-z0-9_.-]`, not
/// starting with `-` (so no flag-smuggling into `runuser`/`--user`, no
/// `/`, whitespace or shell metacharacters). `root`/`0` as the user is
/// refused — a root child is not separation. Returns `Err` for anything
/// else; the daemon refuses to start rather than running attempts under
/// an unintended identity.
pub fn validate_run_as_user(spec: &str) -> anyhow::Result<()> {
    let (user, group) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    for part in std::iter::once(user).chain(group) {
        if part.is_empty() {
            anyhow::bail!(
                "AGENTGRID_RUN_AS_USER={spec:?}: empty user/group part (expected <user>[:<group>])"
            );
        }
        if part.len() > 32 {
            anyhow::bail!("AGENTGRID_RUN_AS_USER={spec:?}: part {part:?} exceeds 32 chars");
        }
        let mut chars = part.chars();
        let first = chars.next().unwrap_or('\0');
        if !(first.is_ascii_alphanumeric() || first == '_') {
            anyhow::bail!(
                "AGENTGRID_RUN_AS_USER={spec:?}: part {part:?} must start with a letter, digit or '_'"
            );
        }
        if !part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
        {
            anyhow::bail!(
                "AGENTGRID_RUN_AS_USER={spec:?}: part {part:?} may only contain [A-Za-z0-9_.-]"
            );
        }
    }
    if user == "root" || user == "0" {
        anyhow::bail!(
            "AGENTGRID_RUN_AS_USER={spec:?}: running the attempt as root is not user separation — refusing"
        );
    }
    Ok(())
}

/// Plan 6.11: the `runuser -u <user> [-g <group>] --` argv head for the
/// native/systemd paths. Empty when no (valid) spec is configured — the
/// spawn-path backstop: an invalid value never reaches `runuser` verbatim
/// even if a caller skipped the startup gate.
fn valid_run_as_prefix() -> Vec<String> {
    let Some(spec) = run_as_spec().filter(|s| validate_run_as_user(s).is_ok()) else {
        return Vec::new();
    };
    let (user, group) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec.as_str(), None),
    };
    // `runuser` (util-linux, present on Tier-1 Debian/Ubuntu) switches uid
    // without PAM/session setup — the minimal tool for a daemon-spawned
    // child. `--` stops flag parsing so the validated program name can
    // never be mistaken for a runuser flag.
    let mut v = vec!["runuser".to_string(), "-u".to_string(), user.to_string()];
    if let Some(g) = group {
        v.push("-g".to_string());
        v.push(g.to_string());
    }
    v.push("--".to_string());
    v
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
            "AGENTGRID_SANDBOX_NETWORK={net}: egress allowlist is not enforceable by the \
             sandbox backends (docker has no native per-CIDR egress filter; bwrap can only \
             unshare the whole netns); refusing to run with a silently unenforced allowlist. \
             Use none/bridge/host, or wire an egress proxy and pass its network"
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
