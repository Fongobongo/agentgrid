//! Bubblewrap backend (ADR 0022): mount masks, namespace flags, probe.

use super::*;

pub(super) fn bwrap_rebind_if_masked(
    prefix: &mut Vec<String>,
    program: &str,
    masks: &[std::path::PathBuf],
) {
    let masked_fixed = ["/tmp/", "/dev/", "/proc/"];
    let under_fixed = masked_fixed.iter().any(|m| program.starts_with(m));
    let under_mask = masks
        .iter()
        .any(|m| std::path::Path::new(program).starts_with(m));
    if !under_fixed && !under_mask {
        return;
    }
    // Only a real file can be bound; a bare name or a missing path is left to
    // fail with bwrap's own message.
    if !std::path::Path::new(program).is_file() {
        return;
    }
    bwrap_insert_ro_bind(prefix, program);
}

/// Insert `--ro-bind <path> <path>` before the argv terminator.
///
/// `bwrap_head` ends with `--`, after which bwrap reads the next argument as
/// the program: appending a bind there made it exec `--ro-bind`.
fn bwrap_insert_ro_bind(prefix: &mut Vec<String>, path: &str) {
    let bind = ["--ro-bind", path, path];
    match prefix.iter().rposition(|a| a == "--") {
        Some(pos) => {
            for (offset, arg) in bind.iter().enumerate() {
                prefix.insert(pos + offset, arg.to_string());
            }
        }
        None => prefix.extend(bind.iter().map(|a| a.to_string())),
    }
}

/// Re-expose the `PATH` directories the masks just hid.
///
/// Masking `$HOME` also hides `~/.cargo/bin`, `~/.local/bin`, `~/.nvm/...` —
/// where the adapter and the toolchain an attempt runs (`cargo test` as a
/// validation command) usually live. Losing them turns a security fix into
/// `cargo: not found` on every attempt of that node. Only the `PATH` entries
/// themselves are bound back, read-only: an executable directory is not where
/// a credential belongs, and `~/.cargo/credentials.toml`, `~/.ssh` and the
/// rest stay hidden because they are siblings, not entries.
///
/// `path_var` is a parameter rather than `std::env::var_os("PATH")` so a test
/// can drive it without mutating the process environment.
pub(super) fn bwrap_rebind_path_dirs(
    prefix: &mut Vec<String>,
    masks: &[std::path::PathBuf],
    path_var: Option<&std::ffi::OsStr>,
) {
    if masks.is_empty() {
        return;
    }
    let Some(path_var) = path_var else {
        return;
    };
    let mut bound: Vec<std::path::PathBuf> = Vec::new();
    for dir in std::env::split_paths(path_var) {
        let Ok(real) = std::fs::canonicalize(&dir) else {
            continue;
        };
        if !real.is_dir() || bound.contains(&real) {
            continue;
        }
        if !masks.iter().any(|m| real.starts_with(m)) {
            continue;
        }
        bound.push(real.clone());
        bwrap_insert_ro_bind(prefix, &real.display().to_string());
    }
}

/// Host directories a bwrap attempt must not be able to read, even read-only.
///
/// `--ro-bind / /` gives the agent everything the daemon user can read: the
/// node credential (`credential.json`), the ADR 0028 X25519 key that unseals
/// adapter env, `~/.ssh`, cloud CLI credentials, and every other attempt's
/// worktree under the workspace root. Read-only does not help — the threat is
/// reading, and a coding agent with shell access exfiltrates what it can read.
///
/// Masking with `--tmpfs` is the cheap half of the fix: it hides exactly these
/// subtrees while leaving `/usr`, `/lib`, `/bin`, `/etc` and the toolchain in
/// place, so nothing has to be re-listed and a missing entry cannot break an
/// attempt. The worktree is bound *after* the masks, so it stays visible even
/// when it lives inside one of them.
///
/// Only absolute, existing paths are masked: bwrap creates a mount point where
/// one already exists, and a nonexistent destination would fail the attempt
/// with `Can't mkdir …: Read-only file system`.
pub(super) fn bwrap_masked_paths(workdir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    let mut consider = |p: std::path::PathBuf| {
        // Canonicalise so a symlinked data dir is masked where it really is.
        let Ok(real) = std::fs::canonicalize(&p) else {
            return;
        };
        if !real.is_absolute() || !real.is_dir() {
            return;
        }
        // Never mask the filesystem root: a worktree directly under `/` makes
        // its parent `/`, and `--tmpfs /` would erase the namespace the next
        // arguments are built on. `parent()` is the portable "is a root" test —
        // a component count is not: on Windows `canonicalize` returns a
        // verbatim path whose components are `[Prefix, RootDir]`, so `D:\`
        // counted as two and the whole drive was masked.
        if real.parent().is_none() {
            return;
        }
        // Masking the worktree itself would hide the only thing the agent is
        // supposed to see (it is re-bound below, but an empty `--tmpfs` over it
        // first is pointless noise in the argv).
        if real == workdir {
            return;
        }
        if !out.contains(&real) {
            out.push(real);
        }
    };
    // The daemon's own data directory: credential, keys, outbox, repo cache.
    let data = std::env::var("AGENTGRID_DATA_DIR").unwrap_or_else(|_| "./agentgrid-data".into());
    let data = std::path::Path::new(&data);
    consider(if data.is_absolute() {
        data.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(data)
    });
    // The workspace root: sibling attempts' worktrees are other tasks' code.
    if let Some(root) = workdir.parent() {
        consider(root.to_path_buf());
    }
    // The daemon user's home: `~/.ssh`, `~/.aws`, `~/.docker/config.json`,
    // `~/.git-credentials`, shell history.
    if let Some(home) = std::env::var_os("HOME") {
        consider(std::path::PathBuf::from(home));
    }
    out
}

/// True when `net` is an `allowlist:` spec and every CIDR in it parses.
/// Egress allowlisting is NOT enforceable via the docker CLI alone (no native
/// per-CIDR egress filter) — see [`net_allowlist_enforceable`].
/// True when a bwrap attempt keeps the host network namespace.
///
/// bwrap can only do all-or-nothing networking (`--unshare-net`), so the
/// mapping is coarser than docker's: `none` (and anything unrecognised, i.e.
/// a task-supplied mode that is not one of the three documented ones) keeps
/// the namespace unshared — strictly more isolated than promised, never less.
///
/// `restricted` is **not** enforceable here and therefore never shares it.
/// bwrap has no per-CIDR filter and no proxy-only network: with `--share-net`
/// the "proxy" is an `HTTP(S)_PROXY` environment variable, which any process
/// can ignore and connect straight to the LAN, the metadata endpoint or the
/// control plane. So `restricted` collapses to `none` (see [`bwrap_head`]) and
/// a node configured for it under bwrap refuses to start (see
/// [`check_startup_config`]) rather than reporting an isolation it does not
/// have. `unrestricted` (and the docker-flavoured spellings `bridge`/`host`)
/// shares the namespace.
pub fn bwrap_shares_network(mode: &str) -> bool {
    matches!(mode, "unrestricted" | "bridge" | "host")
}

/// The `bwrap … --` argv head for [`SandboxKind::Bwrap`]. `--unshare-all`
/// gives a private user/pid/ipc/uts/cgroup namespace; the host root is bound
/// read-only and the worktree is bound writable on top of it (bwrap applies
/// binds in order, so the later bind shadows the read-only root).
///
/// No cgroup limits: `limits` and the `AGENTGRID_SANDBOX_{PIDS_LIMIT,MEMORY,
/// CPUS}` knobs are docker/systemd-only, and the RSS budget poll in the
/// supervisor still bounds the attempt. This is recorded in the heartbeat
/// (`enforced_limits = false` for bwrap) rather than silently claimed.
pub(super) fn bwrap_head(
    workdir: &std::path::Path,
    network_mode: Option<&str>,
    read_only_worktree: bool,
    masks: &[std::path::PathBuf],
) -> Vec<String> {
    let raw = network_mode
        .map(|s| s.to_string())
        .or_else(|| std::env::var("AGENTGRID_SANDBOX_NETWORK").ok())
        .unwrap_or_else(|| "none".to_string());
    // Fail closed for TASK-supplied modes only (same rule as
    // `docker_run_head`): an unknown string from an assignment payload must
    // never widen egress. The operator env fallback stays trusted.
    let mut net = if network_mode.is_some()
        && !matches!(raw.as_str(), "none" | "restricted" | "unrestricted")
    {
        tracing::warn!(mode = %raw, "unknown task network_mode clamped to none");
        "none".to_string()
    } else {
        raw
    };
    // Finding 3: `restricted` under bwrap would be the host network with an
    // environment-variable hint, which is not a restriction. Collapsing to
    // `none` is the honest reading of "restricted" — strictly less egress
    // than asked for, never more. The operator-level setting is refused at
    // startup; this covers the task-supplied one.
    if net == "restricted" {
        tracing::warn!(
            "network_mode=restricted under bwrap has no enforcement (no per-CIDR \
             filter, no proxy-only network); running with --unshare-net instead — \
             use AGENTGRID_SANDBOX=docker for enforced restricted egress"
        );
        net = "none".to_string();
    }
    let wd = workdir.display().to_string();
    let mut v = vec![
        // No orphaned agent tree if the daemon dies, and no TIOCSTI injection
        // back into the daemon's terminal.
        "--die-with-parent".to_string(),
        "--new-session".to_string(),
        "--unshare-all".to_string(),
        // `--unshare-all` alone still leaves a root daemon with CAP_SYS_ADMIN
        // inside the new user namespace: `mount -o remount,rw /` can undo the
        // read-only root and unmount the tmpfs masks. Drop the bounding set so
        // neither the agent nor a nested namespace can regain mount privileges.
        // The startup probe below refuses a bwrap too old for these flags
        // instead of silently shipping the weaker sandbox.
        "--unshare-user".to_string(),
        "--cap-drop".to_string(),
        "ALL".to_string(),
    ];
    if bwrap_shares_network(&net) {
        v.push("--share-net".to_string());
    }
    v.extend(["--ro-bind".to_string(), "/".to_string(), "/".to_string()]);
    v.extend([
        "--dev".to_string(),
        "/dev".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
        "--tmpfs".to_string(),
        "/tmp".to_string(),
    ]);
    // Finding 3: the host root is read-only, which stops writes but not reads —
    // and the agent's job is reading. Mask the daemon's data dir, the workspace
    // root and the service user's home so a shell in the sandbox cannot read
    // the node credential, the sealed-env key, `~/.ssh` or a sibling attempt.
    // Emitted before the worktree bind, so a worktree that lives inside one of
    // them is still visible.
    for mask in masks {
        v.extend(["--tmpfs".to_string(), mask.display().to_string()]);
    }
    // Plan 2.4 (#22a): a verifier's worktree is read-only — same mount, ro.
    // Emitted AFTER the /tmp tmpfs so a worktree that happens to live under
    // /tmp is not hidden by it (later binds win).
    v.push(
        if read_only_worktree {
            "--ro-bind"
        } else {
            "--bind"
        }
        .to_string(),
    );
    v.push(wd.clone());
    v.push(wd.clone());
    v.extend(["--chdir".to_string(), wd]);
    v.extend(bwrap_run_as_prefix());
    v.push("--".to_string());
    v
}

/// bwrap counterpart of [`valid_run_as_prefix`]: `bwrap --uid` takes numbers
/// only (names are not resolved inside a fresh user namespace), so a named
/// spec is refused at startup by [`validate_bwrap_run_as`] and simply never
/// reaches here. Emitting `--uid` inside `--unshare-user` maps the calling
/// user onto the target id, which is exactly the separation `runuser` gives
/// the docker/systemd paths — without needing root.
fn bwrap_run_as_prefix() -> Vec<String> {
    let Some(spec) = run_as_spec().filter(|s| validate_bwrap_run_as(s).is_ok()) else {
        return Vec::new();
    };
    let (uid, gid) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec.as_str(), None),
    };
    let mut v = vec!["--uid".to_string(), uid.to_string()];
    if let Some(g) = gid {
        v.push("--gid".to_string());
        v.push(g.to_string());
    }
    v
}

/// bwrap's form of `AGENTGRID_RUN_AS_USER`: `<uid>[:<gid>]`, numeric only,
/// never 0. A named user (`agentgrid-worker`) is refused instead of being
/// ignored — the daemon must not start believing it separated identities when
/// it did not.
pub fn validate_bwrap_run_as(spec: &str) -> anyhow::Result<()> {
    let (uid, gid) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    let uid: u32 = uid.parse().map_err(|_| {
        anyhow::anyhow!(
            "AGENTGRID_RUN_AS_USER={spec:?}: AGENTGRID_SANDBOX=bwrap can only switch to a \
             NUMERIC uid (bwrap --uid does not resolve names), e.g. 1001:1001 — use \
             AGENTGRID_SANDBOX=docker or systemd for a named user"
        )
    })?;
    if uid == 0 {
        anyhow::bail!(
            "AGENTGRID_RUN_AS_USER={spec:?}: running the attempt as uid 0 is not user \
             separation — refusing"
        );
    }
    if let Some(g) = gid {
        g.parse::<u32>().map_err(|_| {
            anyhow::anyhow!(
                "AGENTGRID_RUN_AS_USER={spec:?}: group {g:?} must be numeric for \
                 AGENTGRID_SANDBOX=bwrap"
            )
        })?;
    }
    Ok(())
}

/// Startup probe for [`SandboxKind::Bwrap`]: the binary must resolve AND a
/// trivial namespace must actually be creatable. `kernel.unprivileged_userns_clone=0`
/// (some hardened kernels) or a seccomp policy that blocks `unshare` makes
/// bwrap fail at exec time, which would otherwise surface as a per-attempt
/// spawn error on every task. Returns the `bwrap --version` line on success.
pub async fn probe_bwrap() -> anyhow::Result<Option<String>> {
    if resolve_binary("bwrap").is_none() {
        return Ok(None);
    }
    // Probe with the exact flags `bwrap_head` uses, not just a namespace:
    // an older bwrap without `--cap-drop` would pass here and only fail (or,
    // worse, run without the capability drop) on every attempt.
    let out = tokio::process::Command::new("bwrap")
        .args([
            "--die-with-parent",
            "--unshare-all",
            "--unshare-user",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/",
            "/",
            "--",
            "true",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "bwrap is installed but cannot run with the required namespace and \
             capability-drop flags ({}): upgrade bubblewrap (>= 0.5), enable \
             unprivileged user namespaces (sysctl kernel.unprivileged_userns_clone=1) \
             or use AGENTGRID_SANDBOX=docker",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v = tokio::process::Command::new("bwrap")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(Some(v.unwrap_or_else(|| "unknown".to_string())))
}
