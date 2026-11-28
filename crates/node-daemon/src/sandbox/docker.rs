//! Docker/Podman backend: command construction, OOM inspection, cleanup.

use super::*;

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
fn worktree_user(workdir: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    // Never follow a symlink here: the same path is bind-mounted read-write,
    // so the uid we derive must be the owner of the directory actually mounted,
    // not of some root-owned target a previous attempt left behind. A link
    // must not silently *raise* privilege to the image's root either, so use
    // the daemon's own uid/gid (non-root on a normal install) instead.
    let md = std::fs::symlink_metadata(workdir).ok()?;
    if md.file_type().is_symlink() {
        tracing::warn!(
            path = %workdir.display(),
            "worktree is a symlink; using the daemon identity for --user"
        );
        return Some(format!("{}:{}", unsafe { libc::getuid() }, unsafe {
            libc::getgid()
        }));
    }
    if !md.is_dir() {
        return None;
    }
    Some(format!("{}:{}", md.uid(), md.gid()))
}

#[cfg(not(unix))]
fn worktree_user(_workdir: &std::path::Path) -> Option<String> {
    None
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
#[allow(clippy::too_many_arguments)]
pub(super) fn docker_run_head(
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
    // Plan 6.11: separate Unix user for the attempt subprocess. `--user` is
    // enforced by the container runtime itself (an unknown user fails the run
    // loudly — fail-closed by construction), so no privilege is needed on the
    // daemon side. Only a validated spec is ever emitted (see
    // `validate_run_as_user`); an invalid value falls back to the daemon
    // user rather than reaching `--user` verbatim.
    let mut numeric_user = false;
    if let Some(spec) = run_as_spec().filter(|s| validate_run_as_user(s).is_ok()) {
        numeric_user = spec.chars().all(|c| c.is_ascii_digit() || c == ':');
        v.push("--user".to_string());
        v.push(spec);
    } else if let Some(spec) = worktree_user(workdir) {
        // No blind fixed uid: the worktree is bind-mounted read-write, so the
        // only safe default is its actual owner. When the daemon and worktree
        // are root-owned this is still root, but never a *different* uid that
        // cannot write the mount.
        v.push("--user".to_string());
        v.push(spec);
        numeric_user = true;
    }
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
    // sandboxed agents. The flag is deliberately value-less: `--env K=V` would
    // publish every adapter API key and sealed-env value in `ps auxww` and
    // `/proc/<pid>/cmdline` for the whole attempt, while `--env K` takes the
    // value from the client's environment, which is already populated.
    for (k, _value) in container_env {
        v.push("--env".to_string());
        v.push(k.clone());
    }
    // Competitor-gap feature (egress firewall): inject the proxy URL so
    // restricted-mode adapters route through the sidecar (which enforces the
    // domain allowlist). Only applied when a restricted attempt attached to
    // the egress network — an unrestricted/none attempt must not inherit it.
    if network_mode == Some("restricted") && egress_proxy_network().is_some() {
        for (key, _) in egress_proxy_env() {
            v.push("--env".to_string());
            v.push(key);
        }
    }
    if numeric_user {
        // A numeric uid has no passwd entry, so the image's HOME is usually
        // unwritable. `/tmp` is always a writable tmpfs — including for
        // verifiers whose worktree is mounted read-only at `/ag`. Emitted
        // last so an operator-supplied `--env HOME` cannot override it.
        v.extend(["--env".to_string(), "HOME=/tmp".to_string()]);
    }
    // ADR 0029 step 5 ("C3"): the gateway may be served over TLS with the
    // operator's own CA, for a CLI that refuses an `http://` base URL. That only
    // works if the sandbox trusts the CA, and every CLI in our images reads a
    // different variable — so one read-only mount, pointed at by all of them.
    //
    // `restricted` only: the gateway is a peer of the sandbox on the egress
    // network, and an unrestricted attempt has no gateway to talk to.
    if network_mode == Some("restricted") {
        if let Some(ca) = env_nonempty(GATEWAY_CA_ENV) {
            v.push("-v".to_string());
            v.push(format!("{ca}:{GATEWAY_CA_IN_CONTAINER}:ro"));
            for key in [
                // OpenSSL-backed clients (curl, python's ssl, most CLIs).
                "SSL_CERT_FILE",
                // python-requests, which ignores SSL_CERT_FILE.
                "REQUESTS_CA_BUNDLE",
                "CURL_CA_BUNDLE",
                // Node, which ignores all of the above.
                "NODE_EXTRA_CA_CERTS",
                // git, which has its own.
                "GIT_SSL_CAINFO",
            ] {
                v.push("--env".to_string());
                v.push(format!("{key}={GATEWAY_CA_IN_CONTAINER}"));
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
