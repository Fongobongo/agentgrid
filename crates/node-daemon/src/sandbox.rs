//! Agent isolation (idea: sandcastle-style sandbox abstraction).
//!
//! A `Sandbox` wraps the command the node would run the agent as, so an agent
//! can be confined to a container (Docker/Podman/microVM) instead of sharing
//! the node's full environment. The default `NoSandbox` runs the agent
//! directly in the worktree (legacy behavior). Configured via
//! `AGENTGRID_SANDBOX` (`none` | `docker`) and `AGENTGRID_SANDBOX_IMAGE`.
//!
//! `sandbox_command` returns the `(program, args)` to spawn: either the raw
//! command, or a hardened `docker run --rm -i --cap-drop=ALL … <image> -- <cmd>`
//! prefix. Both the wrapper path and the ACP path route through it. Docker
//! hardening knobs (plan §25): `AGENTGRID_SANDBOX_NETWORK` (default `none`),
//! `AGENTGRID_SANDBOX_READ_ONLY=1` (read-only root + tmpfs `/tmp`),
//! `AGENTGRID_SANDBOX_PIDS_LIMIT`, `AGENTGRID_SANDBOX_MEMORY`,
//! `AGENTGRID_SANDBOX_CPUS`, `AGENTGRID_SANDBOX_IMAGE_DIGEST` (pin by digest).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxKind {
    None,
    Docker,
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
            _ => SandboxKind::None,
        }
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
pub fn sandbox_prefix(
    kind: SandboxKind,
    workdir: &std::path::Path,
    program: &str,
    network_mode: Option<&str>,
) -> (String, Vec<String>) {
    match kind {
        SandboxKind::None => (program.to_string(), vec![]),
        SandboxKind::Docker => {
            let mut prefix = docker_run_head(workdir, network_mode);
            prefix.push(image_ref());
            prefix.push(program.into());
            ("docker".into(), prefix)
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
/// isolation applied): `none`→`none`, `restricted`→`none` (docker cannot do
/// per-range egress filtering), `unrestricted`→`bridge`, anything else passes
/// through. Mirrors the mapping in [`docker_run_head`].
pub fn resolved_network_mode(mode: &str) -> &'static str {
    match mode {
        "restricted" => "none",
        "unrestricted" => "bridge",
        "none" => "none",
        _ => "none",
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
/// §25: cap-drop, no-new-privileges, network none, optional read-only + tmpfs,
/// optional pids/memory/cpus limits), the worktree mount at `/ag`, and the
/// `--` separator. The caller appends `<image> [program args]` after it.
///
/// Knobs (all optional, env-driven so the sandbox wrapper need not change
/// its call sites to tighten isolation): `AGENTGRID_SANDBOX_NETWORK` (default
/// `none`), `AGENTGRID_SANDBOX_READ_ONLY=1` (read-only root + tmpfs `/tmp`),
/// `AGENTGRID_SANDBOX_PIDS_LIMIT`, `AGENTGRID_SANDBOX_MEMORY`,
/// `AGENTGRID_SANDBOX_CPUS`, `AGENTGRID_SANDBOX_IMAGE_DIGEST` (pins the image
/// by digest when `AGENTGRID_SANDBOX_IMAGE` is a tag).
/// ponytail: limits come from env rather than SpawnRequest.limits so this
/// wrapper need not change signature; plumbing ResourceLimits through is the
/// upgrade path once a real DockerBackend trait owns spawn.
fn docker_run_head(workdir: &std::path::Path, network_mode: Option<&str>) -> Vec<String> {
    let mut v = vec![
        "run".to_string(),
        "--rm".to_string(),
        "-i".to_string(),
        "--cap-drop=ALL".to_string(),
        "--security-opt=no-new-privileges".to_string(),
    ];
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
    // filter), so it maps to `none` — strictly more isolated than promised,
    // never less (a previously raw `--network restricted` would just fail
    // the container). Real LAN-blocking-with-internet needs the egress-proxy
    // upgrade path (same as the allowlist refusal).
    let net = match net.as_str() {
        "restricted" => {
            tracing::debug!(
                "network_mode=restricted maps to --network none (docker has no \
                 per-range egress filter; egress proxy is the upgrade path)"
            );
            "none".to_string()
        }
        "unrestricted" => "bridge".to_string(),
        other => other.to_string(),
    };
    v.push("--network".to_string());
    v.push(net);
    if std::env::var("AGENTGRID_SANDBOX_READ_ONLY")
        .map(|x| x == "1" || x.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        v.push("--read-only".to_string());
        v.push("--tmpfs".to_string());
        v.push("/tmp".to_string());
    }
    if let Ok(p) = std::env::var("AGENTGRID_SANDBOX_PIDS_LIMIT") {
        if !p.is_empty() {
            v.push("--pids-limit".to_string());
            v.push(p);
        }
    }
    if let Ok(m) = std::env::var("AGENTGRID_SANDBOX_MEMORY") {
        if !m.is_empty() {
            v.push("--memory".to_string());
            v.push(m);
        }
    }
    if let Ok(c) = std::env::var("AGENTGRID_SANDBOX_CPUS") {
        if !c.is_empty() {
            v.push("--cpus".to_string());
            v.push(c);
        }
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
    v.push(format!("{}:/ag", workdir.display()));
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

/// Wrap `(program, args)` for the configured sandbox, rooted at `workdir`.
/// `None` returns the command unchanged. `Docker` prefixes with the hardened
/// `docker run … <image> --` head from [`docker_run_head`].
/// ponytail: binds the whole workdir read-write; a stricter mount policy
/// (read-only + separate artifact dir) is the upgrade path once a real
/// DockerBackend trait owns the worktree/artifact mounts.
pub fn sandbox_command(
    kind: SandboxKind,
    program: &str,
    args: &[String],
    workdir: &std::path::Path,
    network_mode: Option<&str>,
) -> (String, Vec<String>) {
    match kind {
        SandboxKind::None => (program.to_string(), args.to_vec()),
        SandboxKind::Docker => {
            let mut out = docker_run_head(workdir, network_mode);
            out.push(image_ref());
            out.push(program.to_string());
            out.extend(args.iter().cloned());
            ("docker".to_string(), out)
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
    let out = tokio::process::Command::new(&runtime)
        .args([
            "run",
            "--rm",
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
    let label = format!("agentgrid.node={node_id}");
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
        );
        assert_eq!(p, "adapter-x");
        assert!(a.is_empty());
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
        let (p, a) = sandbox_prefix(SandboxKind::Docker, std::path::Path::new("/w"), "c", None);
        clear_sandbox_env();
        assert_eq!(p, "docker");
        // image + program are the last two; image must carry the digest pin.
        assert_eq!(a[a.len() - 2], "img:1@sha256:deadbeef");
        assert_eq!(a[a.len() - 1], "c");
        // An already-digested ref is left untouched.
        std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1@sha256:f00d");
        let (_, a2) = sandbox_prefix(SandboxKind::Docker, std::path::Path::new("/w"), "c", None);
        clear_sandbox_env();
        assert_eq!(a2[a2.len() - 2], "img:1@sha256:f00d");
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
        std::env::set_var("AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX", "1");
        let remove = unsafe_env_guard(SandboxKind::None);
        assert!(remove.is_empty(), "explicit override keeps the unsafe env");
        std::env::remove_var("AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX");
    }
}
