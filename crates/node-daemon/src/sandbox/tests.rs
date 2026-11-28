use super::*;
use std::sync::Mutex;

/// Process-global env vars are shared across tests; serialize the
/// sandbox-env mutators so parallel runs cannot race on
/// `AGENTGRID_SANDBOX_*` (pre-existing flake: docker_pins_image_by_digest
/// vs docker_opts_read_only_and_resource_limits).
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Finding 16: an unknown `AGENTGRID_SANDBOX` must not quietly mean
/// "no sandbox" — that is the one setting a typo must never weaken.
#[test]
fn an_unknown_sandbox_value_is_an_error_not_none() {
    assert_eq!(SandboxKind::parse(None), Ok(SandboxKind::None));
    assert_eq!(SandboxKind::parse(Some("")), Ok(SandboxKind::None));
    assert_eq!(SandboxKind::parse(Some("none")), Ok(SandboxKind::None));
    assert_eq!(SandboxKind::parse(Some("off")), Ok(SandboxKind::None));
    assert_eq!(SandboxKind::parse(Some("Docker")), Ok(SandboxKind::Docker));
    assert_eq!(
        SandboxKind::parse(Some(" podman ")),
        Ok(SandboxKind::Docker)
    );
    assert_eq!(
        SandboxKind::parse(Some("systemd-scope")),
        Ok(SandboxKind::Systemd)
    );
    assert_eq!(
        SandboxKind::parse(Some("bubblewrap")),
        Ok(SandboxKind::Bwrap)
    );
    for typo in ["dokcer", "bwrap:", "docker;rm -rf", "0"] {
        assert!(
            SandboxKind::parse(Some(typo)).is_err(),
            "{typo:?} must not parse"
        );
    }
}

#[test]
fn none_passthrough() {
    // No sandbox and no run-as user → identity. Lock + clear: sibling
    // tests set AGENTGRID_RUN_AS_USER under ENV_LOCK and the process
    // env is shared.
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    // Lock + clear: AGENTGRID_RUN_AS_USER is process-global (see above).
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        "AGENTGRID_RUN_AS_USER",
        "AGENTGRID_SANDBOX",
        GATEWAY_CA_ENV,
    ] {
        std::env::remove_var(k);
    }
}

#[test]
fn docker_pins_image_by_digest() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let image = std::env::var("AGENTGRID_SANDBOX_IMAGE").unwrap_or_else(|_| "ubuntu:24.04".into());
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

/// Review finding 5: `--env K=V` published every adapter API key and
/// sealed-env value in `ps auxww` and `/proc/<pid>/cmdline` for the whole
/// attempt. The value must travel through the docker client's environment.
#[test]
fn docker_env_flags_carry_names_only() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
    let env = vec![
        (
            "ANTHROPIC_API_KEY".to_string(),
            "sk-secret-value".to_string(),
        ),
        ("OPENAI_API_KEY".to_string(), "sk-other-secret".to_string()),
    ];
    let (_, a) = sandbox_command(
        SandboxKind::Docker,
        "c",
        &[],
        std::path::Path::new("/w"),
        None,
        false,
        None,
        &env,
        None,
    );
    clear_sandbox_env();
    assert!(
        !a.iter()
            .any(|x| x.contains("sk-secret") || x.contains("sk-other")),
        "a secret reached the argv: {a:?}"
    );
    assert!(
        a.windows(2)
            .any(|w| w[0] == "--env" && w[1] == "ANTHROPIC_API_KEY"),
        "--env must name the variable without its value: {a:?}"
    );
}

#[test]
fn task_network_mode_maps_to_docker_native_networks() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    // Proxy env is injected for restricted attempts on the egress network —
    // by name only: the value rides the docker client's environment, so a
    // proxy URL carrying `user:pass@` never lands in argv (review finding 5).
    let env_names: Vec<&String> = a
        .windows(2)
        .filter(|w| w[0] == "--env")
        .map(|w| &w[1])
        .collect();
    assert!(
        env_names.contains(&&"HTTP_PROXY".to_string()),
        "restricted must forward HTTP_PROXY: {a:?}"
    );
    assert!(
        env_names.contains(&&"HTTPS_PROXY".to_string()),
        "restricted must forward HTTPS_PROXY: {a:?}"
    );
    assert!(
        !a.iter().any(|x| x.contains("egress:3128")),
        "the proxy URL must not reach argv: {a:?}"
    );

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
        !a2.windows(2)
            .any(|w| w[0] == "--env" && w[1] == "HTTP_PROXY"),
        "unrestricted attempt must not inherit the egress proxy env: {a2:?}"
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

/// ADR 0029 C3: the sandbox must trust the CA that signed the gateway's
/// certificate. Every CLI in our images reads a different variable, so the
/// one file is pointed at by all of them; missing any one of them is a
/// connection that fails with an unknown-issuer error naming nothing useful.
#[test]
fn docker_mounts_the_gateway_ca_and_points_every_trust_store_at_it() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
    std::env::set_var(GATEWAY_CA_ENV, "/host/gw-ca.pem");
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
    assert!(
        a.contains(&format!("/host/gw-ca.pem:{GATEWAY_CA_IN_CONTAINER}:ro")),
        "the CA must be mounted read-only: {a:?}"
    );
    for key in [
        "SSL_CERT_FILE",
        "REQUESTS_CA_BUNDLE",
        "CURL_CA_BUNDLE",
        "NODE_EXTRA_CA_CERTS",
        "GIT_SSL_CAINFO",
    ] {
        assert!(
            a.contains(&format!("{key}={GATEWAY_CA_IN_CONTAINER}")),
            "{key} must point at the mounted CA: {a:?}"
        );
    }
}

/// The gateway is a peer of the sandbox only on the egress network. An
/// unrestricted attempt has no gateway to talk to, so a CA there would be a
/// mount the agent could read for no reason.
#[test]
fn the_gateway_ca_is_not_mounted_outside_restricted_mode() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
    std::env::set_var(GATEWAY_CA_ENV, "/host/gw-ca.pem");
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
        !a.iter().any(|x| x.contains("gw-ca.pem")),
        "no CA mount without restricted networking: {a:?}"
    );
    assert!(!a.iter().any(|x| x.starts_with("SSL_CERT_FILE=")));
}

#[test]
fn unsafe_guard_strips_unset_env_when_unsandboxed() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX", "1");
    let remove = unsafe_env_guard(SandboxKind::None);
    assert!(remove.is_empty(), "explicit override keeps the unsafe env");
    std::env::remove_var("AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX");
}

// ---- Plan 6.11: separate Unix user for the attempt subprocess ----

#[test]
fn run_as_spec_validation_accepts_users_groups_and_uids() {
    for ok in [
        "worker",
        "agentgrid-worker",
        "nobody:nogroup",
        "worker:staff",
        "1001",
        "1001:1001",
        "a.b-c_d",
    ] {
        assert!(validate_run_as_user(ok).is_ok(), "{ok} must pass");
    }
}

#[test]
fn run_as_spec_validation_rejects_garbage_and_root() {
    for bad in [
        "",
        ":",
        ":group",
        "user:",
        "root",
        "0",
        "root:staff",
        "0:0",
        "a/b",
        "a;b",
        "a b",
        "-worker",
        "work er",
        "worker:sta ff",
        "a`id`",
        "a$(id)",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", // 33 chars
    ] {
        assert!(
            validate_run_as_user(bad).is_err(),
            "{bad:?} must be rejected"
        );
    }
}

#[test]
fn docker_run_emits_user_when_configured() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
    std::env::set_var("AGENTGRID_RUN_AS_USER", "worker:staff");
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
    assert_eq!(a[at("--user")], "worker:staff");
}

#[cfg(unix)]
#[test]
fn docker_defaults_to_the_worktree_owner() {
    use std::os::unix::fs::MetadataExt;
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
    let dir = std::env::temp_dir().join(format!("ag-docker-user-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let md = std::fs::metadata(&dir).unwrap();
    let expected = format!("{}:{}", md.uid(), md.gid());
    let (_, a) = sandbox_command(
        SandboxKind::Docker,
        "c",
        &[],
        &dir,
        None,
        false,
        None,
        &[],
        None,
    );
    clear_sandbox_env();
    let at = a
        .iter()
        .position(|x| x == "--user")
        .expect("default --user")
        + 1;
    assert_eq!(a[at], expected);
    assert!(
        a.windows(2).any(|w| w == ["--env", "HOME=/tmp"]),
        "a numeric uid needs a writable HOME: {a:?}"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[cfg(unix)]
#[test]
fn derived_home_is_emitted_after_operator_env() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
    std::env::set_var("AGENTGRID_ADAPTER_ENV", "HOME=/host-home");
    let dir = std::env::temp_dir().join(format!("ag-docker-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let (_, a) = sandbox_command(
        SandboxKind::Docker,
        "c",
        &[],
        &dir,
        None,
        false,
        None,
        &[],
        None,
    );
    clear_sandbox_env();
    let homes: Vec<&str> = a
        .windows(2)
        .filter(|w| w[0] == "--env" && (w[1] == "HOME" || w[1].starts_with("HOME=")))
        .map(|w| w[1].as_str())
        .collect();
    assert_eq!(homes.last().copied(), Some("HOME=/tmp"), "{homes:?}");
    std::fs::remove_dir_all(dir).ok();
}

#[cfg(unix)]
#[test]
fn a_symlinked_worktree_does_not_set_a_derived_user() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
    let base = std::env::temp_dir().join(format!("ag-docker-link-{}", uuid::Uuid::new_v4()));
    let real = base.join("real");
    let link = base.join("link");
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let (_, a) = sandbox_command(
        SandboxKind::Docker,
        "c",
        &[],
        &link,
        None,
        false,
        None,
        &[],
        None,
    );
    clear_sandbox_env();
    let expected = format!("{}:{}", unsafe { libc::getuid() }, unsafe {
        libc::getgid()
    });
    let at = a.iter().position(|x| x == "--user").expect("--user") + 1;
    assert_eq!(
        a[at], expected,
        "a symlink must use the daemon identity, not the image root"
    );
    std::fs::remove_dir_all(base).ok();
}

#[test]
fn docker_run_omits_user_on_unresolved_worktree_and_garbage_spec() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    std::env::set_var("AGENTGRID_SANDBOX_IMAGE", "img:1");
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
    assert!(
        !a.iter().any(|x| x == "--user"),
        "no --user without configuration: {a:?}"
    );
    // Backstop: an invalid spec never reaches --user verbatim even if a
    // caller skipped the startup gate.
    std::env::set_var("AGENTGRID_RUN_AS_USER", "a;b");
    let (_, a2) = sandbox_command(
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
        !a2.iter().any(|x| x == "--user"),
        "invalid spec must not reach --user: {a2:?}"
    );
}

#[test]
fn none_prefix_wraps_runuser_when_configured() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    std::env::set_var("AGENTGRID_RUN_AS_USER", "worker");
    let (p, a) = sandbox_prefix(
        SandboxKind::None,
        std::path::Path::new("/w"),
        "adapter-mock",
        None,
        false,
        None,
        &[],
        None,
    );
    clear_sandbox_env();
    assert_eq!(p, "runuser");
    assert_eq!(a[0], "-u");
    assert_eq!(a[1], "worker");
    assert_eq!(a[2], "--");
    assert_eq!(a[3], "adapter-mock");
}

#[test]
fn systemd_prefix_wraps_runuser_inside_scope() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    std::env::set_var("AGENTGRID_RUN_AS_USER", "1001:1001");
    let (p, a) = sandbox_command(
        SandboxKind::Systemd,
        "adapter-mock",
        &["--prompt".into(), "hi".into()],
        std::path::Path::new("/w"),
        None,
        false,
        Some("agentgrid-att1"),
        &[],
        None,
    );
    clear_sandbox_env();
    assert_eq!(p, "systemd-run");
    // Scope head first (cgroup still applies), runuser inside it.
    let ru = a
        .iter()
        .position(|x| x == "runuser")
        .expect("runuser missing");
    assert!(a[..ru].contains(&"--scope".to_string()));
    assert_eq!(a[ru + 1], "-u");
    assert_eq!(a[ru + 2], "1001");
    assert_eq!(a[ru + 3], "-g");
    assert_eq!(a[ru + 4], "1001");
    assert_eq!(a[ru + 5], "--");
    assert_eq!(a[ru + 6], "adapter-mock");
}

// ---- Stage 12 / ADR 0003: per-attempt resource limits ----

use agentgrid_adapters::ResourceLimits;

#[test]
fn per_attempt_limits_override_env_knobs() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
fn sandbox_kind_garbage_is_refused_at_startup_not_silently_none() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for v in ["", "none"] {
        std::env::set_var("AGENTGRID_SANDBOX", v);
        assert_eq!(
            SandboxKind::from_env(),
            SandboxKind::None,
            "'{v}' is an explicit no-sandbox request"
        );
    }
    // `jail`/`kvm` are not backends: they must never select isolation, and
    // since finding 16 they are a startup error rather than a silent None.
    for v in ["jail", "kvm"] {
        std::env::set_var("AGENTGRID_SANDBOX", v);
        assert_eq!(
            SandboxKind::from_env(),
            SandboxKind::None,
            "'{v}' must never select an isolation backend"
        );
        assert!(
            SandboxKind::parse(Some(v)).is_err(),
            "'{v}' must be refused at startup"
        );
    }
    std::env::remove_var("AGENTGRID_SANDBOX");
}

// ---- ADR 0022: bubblewrap backend -------------------------------------

#[test]
fn sandbox_kind_parses_bwrap_aliases() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for v in ["bwrap", "BWRAP", " bubblewrap ", "userns"] {
        std::env::set_var("AGENTGRID_SANDBOX", v);
        assert_eq!(SandboxKind::from_env(), SandboxKind::Bwrap, "{v}");
    }
    std::env::remove_var("AGENTGRID_SANDBOX");
}

/// The namespace flags are the whole point of the backend: without
/// `--unshare-all` the agent shares the host pid/ipc/uts namespaces and
/// the "sandbox" is a bind-mount decoration.
#[test]
fn bwrap_unshares_every_namespace_and_binds_the_worktree() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    let (prog, args) = sandbox_command(
        SandboxKind::Bwrap,
        "claude",
        &["--acp".into()],
        std::path::Path::new("/w"),
        Some("none"),
        false,
        None,
        &[],
        None,
    );
    assert_eq!(prog, "bwrap");
    for flag in [
        "--die-with-parent",
        "--new-session",
        "--unshare-all",
        "--unshare-user",
        "--cap-drop",
    ] {
        assert!(args.contains(&flag.to_string()), "missing {flag}: {args:?}");
    }
    assert!(
        args.windows(2).any(|w| w == ["--cap-drop", "ALL"]),
        "the capability bounding set must be empty: {args:?}"
    );
    // Read-only host root, writable worktree shadowing it, then the
    // private /tmp, /dev and /proc.
    let root = args.windows(3).position(|w| w[0] == "--ro-bind").unwrap();
    assert_eq!(&args[root..root + 3], ["--ro-bind", "/", "/"]);
    let wd = args.windows(3).position(|w| w[0] == "--bind").unwrap();
    assert_eq!(&args[wd..wd + 3], ["--bind", "/w", "/w"]);
    assert!(wd > root, "the worktree bind must come after the ro root");
    assert!(args.windows(2).any(|w| w == ["--chdir", "/w"]));
    assert!(args.windows(2).any(|w| w == ["--proc", "/proc"]));
    assert!(args.windows(2).any(|w| w == ["--dev", "/dev"]));
    assert!(args.windows(2).any(|w| w == ["--tmpfs", "/tmp"]));
    // `--` terminates flag parsing; program + args follow it verbatim.
    // The program is resolved against the host PATH (bwrap's namespace
    // has none), so compare against the same resolution.
    let sep = args.iter().position(|a| a == "--").unwrap();
    let expected = crate::capabilities::resolve_in_path("claude")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "claude".to_string());
    assert_eq!(&args[sep + 1..], [expected.as_str(), "--acp"]);
}

#[test]
fn bwrap_network_mode_decides_whether_net_is_shared() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    let shared = |mode: &str| {
        let (_, a) = sandbox_prefix(
            SandboxKind::Bwrap,
            std::path::Path::new("/w"),
            "c",
            Some(mode),
            false,
            None,
            &[],
            None,
        );
        a.contains(&"--share-net".to_string())
    };
    // `none` keeps the netns unshared; `unrestricted` shares it.
    assert!(!shared("none"));
    assert!(shared("unrestricted"));
    // Finding 3: `restricted` never shares it, proxy URL or not. A shared
    // net with an HTTP_PROXY variable is unfiltered internet — the variable
    // is a hint, not a firewall.
    assert!(!shared("restricted"));
    std::env::set_var("AGENTGRID_SANDBOX_EGRESS_PROXY", "http://egress:3128");
    assert!(
        !shared("restricted"),
        "a proxy URL must not turn restricted into --share-net under bwrap"
    );
    std::env::remove_var("AGENTGRID_SANDBOX_EGRESS_PROXY");
    // An unknown TASK mode must never widen egress.
    assert!(!shared("host"));
}

#[test]
fn bwrap_restricted_mode_does_not_pretend_to_have_a_proxy() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    std::env::set_var("AGENTGRID_SANDBOX_EGRESS_PROXY", "http://egress:3128");
    let (_, a) = sandbox_prefix(
        SandboxKind::Bwrap,
        std::path::Path::new("/w"),
        "c",
        Some("restricted"),
        false,
        None,
        &[],
        None,
    );
    // No `--share-net` and no proxy hint: a `--setenv HTTPS_PROXY` on a
    // network namespace that was never unshared was the lie. The attempt
    // runs with no egress at all instead.
    assert!(!a.iter().any(|x| x == "--share-net"));
    assert!(!a.iter().any(|x| x == "--setenv"));
    std::env::remove_var("AGENTGRID_SANDBOX_EGRESS_PROXY");
}

/// Finding 3: `--ro-bind / /` let the agent read the node credential, the
/// sealed-env key, `~/.ssh` and sibling attempts. Those subtrees are masked
/// with a tmpfs, and the worktree bind comes after them so it stays
/// visible.
#[test]
fn bwrap_masks_the_host_directories_an_agent_must_not_read() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    let tmp = std::env::temp_dir().join(format!("ag-mask-{}", std::process::id()));
    let data = tmp.join("data");
    let ws = tmp.join("workspaces");
    let work = ws.join("attempt-1");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::env::set_var("AGENTGRID_DATA_DIR", &data);

    let masks = bwrap_masked_paths(&work);
    let data = std::fs::canonicalize(&data).unwrap();
    let ws = std::fs::canonicalize(&ws).unwrap();
    let work = std::fs::canonicalize(&work).unwrap();
    assert!(
        masks.contains(&data),
        "the data dir must be masked: {masks:?}"
    );
    assert!(
        masks.contains(&ws),
        "the workspace root (sibling attempts) must be masked: {masks:?}"
    );
    assert!(
        !masks.contains(&work),
        "the worktree itself is what the agent works in: {masks:?}"
    );
    // Never the root: `--tmpfs /` would erase the namespace.
    assert!(!masks.iter().any(|m| m.components().count() < 2));

    let (_, a) = sandbox_prefix(
        SandboxKind::Bwrap,
        &work,
        "c",
        Some("none"),
        false,
        None,
        &[],
        None,
    );
    let mask_at = a
        .windows(2)
        .position(|w| w[0] == "--tmpfs" && w[1] == data.display().to_string())
        .expect("the data dir is masked");
    let bind_at = a
        .windows(3)
        .position(|w| {
            w[0] == "--bind"
                && w[1] == work.display().to_string()
                && w[2] == work.display().to_string()
        })
        .expect("the worktree is bound");
    assert!(
        mask_at < bind_at,
        "masks must precede the worktree bind, or they hide it"
    );

    std::env::remove_var("AGENTGRID_DATA_DIR");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Masking `$HOME` also hides `~/.cargo/bin` and `~/.local/bin`, where the
/// adapter and the toolchain an attempt runs usually live. The `PATH`
/// entries that the masks swallowed are bound back read-only — the entries
/// only, so `~/.ssh` and `~/.cargo/credentials.toml` stay hidden as
/// siblings rather than as directories on `PATH`.
#[test]
fn path_entries_swallowed_by_a_mask_are_bound_back() {
    let tmp = std::env::temp_dir().join(format!("ag-pathbind-{}", std::process::id()));
    let home = tmp.join("home");
    let bin = home.join(".cargo").join("bin");
    let outside = tmp.join("usr-bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let home = std::fs::canonicalize(&home).unwrap();
    let bin = std::fs::canonicalize(&bin).unwrap();
    let outside = std::fs::canonicalize(&outside).unwrap();

    let path_var = std::env::join_paths([outside.as_path(), bin.as_path()]).unwrap();
    let mut prefix: Vec<String> = vec!["--ro-bind".into(), "/".into(), "/".into(), "--".into()];
    bwrap_rebind_path_dirs(
        &mut prefix,
        std::slice::from_ref(&home),
        Some(path_var.as_os_str()),
    );

    let bin_s = bin.display().to_string();
    let at = prefix
        .windows(3)
        .position(|w| w[0] == "--ro-bind" && w[1] == bin_s && w[2] == bin_s)
        .expect("the hidden PATH entry is bound back");
    // Before the terminator, or bwrap tries to exec the flag.
    assert!(
        at + 3 <= prefix.iter().rposition(|a| a == "--").unwrap(),
        "the bind must precede the argv terminator: {prefix:?}"
    );
    assert!(
        !prefix.iter().any(|a| a == &outside.display().to_string()),
        "a PATH entry outside the masks needs no bind: {prefix:?}"
    );
    // Nothing to do without masks — the common case on a docker node.
    let mut plain: Vec<String> = vec!["--".into()];
    bwrap_rebind_path_dirs(&mut plain, &[], Some(path_var.as_os_str()));
    assert_eq!(plain, vec!["--".to_string()]);

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Finding 3: the answer to "is egress isolated" depended on the mode only,
/// so a `restricted` attempt on a bwrap node read as isolated while it was
/// running on the host network.
#[test]
fn egress_isolation_depends_on_the_sandbox_kind() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    // bwrap: only `none` is enforced (it unshares the net namespace).
    assert!(effective_egress_isolated(SandboxKind::Bwrap, "none"));
    assert!(!effective_egress_isolated(SandboxKind::Bwrap, "restricted"));
    assert!(!effective_egress_isolated(
        SandboxKind::Bwrap,
        "unrestricted"
    ));
    // docker: `restricted` with no proxy network is its own `--network
    // none`; with a network and a URL the proxy filters.
    assert!(effective_egress_isolated(SandboxKind::Docker, "none"));
    assert!(effective_egress_isolated(SandboxKind::Docker, "restricted"));
    std::env::set_var("AGENTGRID_SANDBOX_EGRESS_NETWORK", "egress");
    assert!(
        !effective_egress_isolated(SandboxKind::Docker, "restricted"),
        "a proxy network without a proxy URL is unfiltered internet"
    );
    std::env::set_var("AGENTGRID_SANDBOX_EGRESS_PROXY", "http://egress:3128");
    assert!(effective_egress_isolated(SandboxKind::Docker, "restricted"));
    clear_sandbox_env();
    // A scope and a bare process share the host network namespace.
    assert!(!effective_egress_isolated(SandboxKind::Systemd, "none"));
    assert!(!effective_egress_isolated(SandboxKind::None, "none"));
}

/// Finding 3: an operator who asked for enforced `restricted` egress under
/// bwrap must be told it cannot be done, not silently given the host
/// network.
#[test]
fn bwrap_refuses_restricted_egress_at_startup() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    std::env::set_var("AGENTGRID_SANDBOX", "bwrap");
    std::env::set_var("AGENTGRID_SANDBOX_NETWORK", "restricted");
    let err = check_startup_config(true).expect_err("restricted+bwrap must not start");
    let msg = format!("{err}");
    assert!(
        msg.contains("AGENTGRID_SANDBOX=docker"),
        "the message must name the backend that can do it: {msg}"
    );
    // `none` under bwrap is fine.
    std::env::set_var("AGENTGRID_SANDBOX_NETWORK", "none");
    check_startup_config(true).expect("none under bwrap is enforceable");
    clear_sandbox_env();
}

#[test]
fn bwrap_read_only_worktree_uses_a_ro_bind() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    let (_, a) = sandbox_prefix(
        SandboxKind::Bwrap,
        std::path::Path::new("/w"),
        "c",
        Some("none"),
        true,
        None,
        &[],
        None,
    );
    // The host root and the worktree are read-only; the worktree is the
    // second of the two. Later `--ro-bind`s (the `PATH` entries a mask
    // swallowed) are host-dependent and not part of this contract.
    let ro: Vec<usize> = a
        .iter()
        .enumerate()
        .filter(|(_, x)| x.as_str() == "--ro-bind")
        .map(|(i, _)| i)
        .collect();
    assert!(ro.len() >= 2, "{a:?}");
    assert_eq!(&a[ro[1]..ro[1] + 3], ["--ro-bind", "/w", "/w"]);
    assert!(!a.iter().any(|x| x == "--bind"));
}

#[test]
fn bwrap_prefix_and_command_agree() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    let (p1, a1) = sandbox_prefix(
        SandboxKind::Bwrap,
        std::path::Path::new("/w"),
        "claude",
        Some("none"),
        false,
        None,
        &[],
        None,
    );
    let (p2, a2) = sandbox_command(
        SandboxKind::Bwrap,
        "claude",
        &["--acp".into()],
        std::path::Path::new("/w"),
        Some("none"),
        false,
        None,
        &[],
        None,
    );
    assert_eq!(p1, p2);
    // `sandbox_prefix` ends with the program; `sandbox_command` continues
    // with the args after it — so the heads must be identical.
    //
    // bwrap resolves a bare program name against the host PATH (the
    // namespace has no PATH), so the expected value depends on whether
    // the host has `claude` installed. Compare against the same
    // resolution instead of hard-coding the bare name.
    let expected = crate::capabilities::resolve_in_path("claude")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "claude".to_string());
    assert_eq!(a1.last().map(String::as_str), Some(expected.as_str()));
    assert_eq!(a2[..a2.len() - 2], a1[..a1.len() - 1]);
    assert_eq!(&a2[a2.len() - 2..], [expected.as_str(), "--acp"]);
}

#[test]
fn bwrap_run_as_accepts_numbers_and_refuses_names_and_root() {
    assert!(validate_bwrap_run_as("1001").is_ok());
    assert!(validate_bwrap_run_as("1001:1001").is_ok());
    for bad in ["worker", "worker:worker", "0", "0:0", "1001:x", ""] {
        assert!(
            validate_bwrap_run_as(bad).is_err(),
            "{bad:?} must be refused"
        );
    }
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear_sandbox_env();
    std::env::set_var("AGENTGRID_RUN_AS_USER", "1001:1002");
    let (_, a) = sandbox_prefix(
        SandboxKind::Bwrap,
        std::path::Path::new("/w"),
        "c",
        Some("none"),
        false,
        None,
        &[],
        None,
    );
    assert!(a.windows(2).any(|w| w == ["--uid", "1001"]));
    assert!(a.windows(2).any(|w| w == ["--gid", "1002"]));
    // A named spec never reaches the argv (startup refuses it instead).
    std::env::set_var("AGENTGRID_RUN_AS_USER", "worker");
    let (_, a) = sandbox_prefix(
        SandboxKind::Bwrap,
        std::path::Path::new("/w"),
        "c",
        Some("none"),
        false,
        None,
        &[],
        None,
    );
    assert!(!a.iter().any(|x| x == "--uid"));
}

/// The second half of the bwrap path bug: `bwrap_head` replaces `/tmp`
/// with a tmpfs, so an adapter installed under `/tmp` is present on the
/// host and gone inside the namespace. The rebind is only emitted for a
/// real file under a masked path — everything else keeps the plain vector.
#[test]
fn bwrap_rebinds_only_a_real_program_under_a_masked_path() {
    let mut v = Vec::new();
    bwrap_rebind_if_masked(&mut v, "/usr/local/bin/adapter-mock", &[]);
    assert!(v.is_empty(), "unexpected bind for an unmasked path: {v:?}");

    let mut v = Vec::new();
    bwrap_rebind_if_masked(&mut v, "/tmp/nope/not-a-file", &[]);
    assert!(v.is_empty(), "bound a path that does not exist: {v:?}");

    let dir = std::env::temp_dir();
    let real = dir.join("agentgrid-rebind-probe");
    std::fs::write(&real, b"x").expect("write probe");
    let mut v = Vec::new();
    bwrap_rebind_if_masked(&mut v, &real.display().to_string(), &[]);
    // Windows temp is not under /tmp, so this only asserts on unix.
    if real.display().to_string().starts_with("/tmp/") {
        assert_eq!(
            v,
            vec![
                "--ro-bind".to_string(),
                real.display().to_string(),
                real.display().to_string()
            ]
        );
    }

    // `bwrap_head` ends with `--`; a bind appended after it became the
    // program ("bwrap: execvp --ro-bind"). It has to go before.
    let mut v = vec!["--tmpfs".to_string(), "--".to_string()];
    bwrap_rebind_if_masked(&mut v, &real.display().to_string(), &[]);
    if real.display().to_string().starts_with("/tmp/") {
        assert_eq!(v.last().map(String::as_str), Some("--"));
        assert_eq!(v[1], "--ro-bind");
        assert_eq!(v.len(), 5);
    }
    let _ = std::fs::remove_file(&real);
}

#[test]
fn only_container_backends_inherit_the_daemon_env() {
    // The eval probe and `ProcessBackend::spawn` key the clean-env
    // decision off this: a bwrap child runs on the host, so inheriting
    // the daemon's secrets would defeat the sandbox.
    assert!(!SandboxKind::None.inherits_daemon_env());
    assert!(!SandboxKind::Bwrap.inherits_daemon_env());
    assert!(SandboxKind::Docker.inherits_daemon_env());
    assert!(SandboxKind::Systemd.inherits_daemon_env());
}

/// A bwrap child gets the curated environment, not the daemon's, so a bare
/// program name has no PATH to resolve against inside the namespace. On a
/// real Debian node the first bwrap attempt died with
/// `bwrap: execvp adapter-mock: No such file or directory` — the argument
/// vector looked right, which is why the unit tests never saw it.
#[test]
fn bwrap_hands_over_the_host_path_not_a_bare_name() {
    let known = if cfg!(windows) { "cmd" } else { "sh" };
    let (bin, args) = sandbox_command(
        SandboxKind::Bwrap,
        known,
        &[],
        std::path::Path::new("/work"),
        None,
        false,
        None,
        &[],
        None,
    );
    assert_eq!(bin, "bwrap");
    let program = args.last().expect("program missing");
    assert!(
        std::path::Path::new(program).is_absolute(),
        "program {program:?} is not absolute — execvp inside the sandbox has no PATH"
    );
    assert!(
        !args.iter().any(|a| a == known),
        "bare name {known:?} still in the argument vector"
    );
}
