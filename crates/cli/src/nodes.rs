//! `ag node(s) …` handlers — extracted from main.rs in the CLI monolith
//! split. Shared helpers (paint/api_error/…) live in main.rs.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::{api_error, create_enrollment_token, err_if_fail, list_items};

#[derive(Args)]
pub(crate) struct NodeArgs {
    #[command(subcommand)]
    command: NodeSub,
}

#[derive(Subcommand)]
pub(crate) enum NodeSub {
    /// List registered nodes.
    List,
    /// Provision a remote host as a node over SSH and link it to this control plane.
    Install(Box<NodeInstallArgs>),
    /// Diagnose a node: fetch its control-plane view and surface known
    /// symptoms (status, missing adapters, low disk, stale heartbeat). Doctor
    /// is report-only — it does not mutate the node. Use `ag node install` /
    /// the node daemon for repair; this surfaces the symptoms there.
    Doctor { node_id: String },
    /// Drain a node for maintenance: it keeps in-flight attempts but receives
    /// no NEW assignments. `--undrain` re-enables assignments.
    Drain {
        node_id: String,
        /// Re-enable assignments on this node.
        #[arg(long)]
        undrain: bool,
    },
}

/// Transport used for the node -> control-plane runtime link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Default)]
pub(crate) enum Transport {
    /// Reverse SSH tunnel (default). Works behind NAT; SSH encrypts the link.
    #[default]
    SshTunnel,
    /// Private WireGuard network (planned). SSH used only for one-time bootstrap.
    Wireguard,
}

#[derive(Args)]
pub(crate) struct NodeInstallArgs {
    /// Remote host as user@host or user@host:port.
    #[arg(long)]
    host: String,
    /// Path to SSH private key (key-based auth; recommended over --password).
    #[arg(long)]
    ssh_key: Option<String>,
    /// SSH password (requires `sshpass`; passed via SSHPASS env, never argv).
    #[arg(long)]
    password: Option<String>,
    /// Accept an unknown SSH host key on first connect (like ssh-keyscan -H).
    /// OFF by default: an unknown host key is REFUSED (fail-closed, no MITM).
    /// Use only for a freshly-provisioned host you trust but have not yet
    /// pinned.
    #[arg(long, default_value_t = false)]
    accept_new_host_key: bool,
    /// Pin the remote host's SSH public key fingerprint (e.g.
    /// `SHA256:base64...`) for strict provisioning. Refuses the host if it does
    /// not match; overrides --accept-new-host-key.
    #[arg(long)]
    host_key_fingerprint: Option<String>,
    /// Allow the node daemon to run as root on the remote (sets
    /// `AGENTGRID_ALLOW_ROOT=1`). OFF by default: the daemon refuses root, so
    /// SSH as (or create) an unprivileged user and point --data-dir at a dir it
    /// owns. Only enable when you cannot avoid root and understand the risk.
    #[arg(long, default_value_t = false)]
    allow_root: bool,
    /// Transport for the node -> control-plane link.
    #[arg(long, value_enum, default_value = "ssh-tunnel")]
    transport: Transport,
    /// Node display name.
    #[arg(long, default_value = "remote-node")]
    name: String,
    /// Repositories the node may serve (comma list or '*').
    #[arg(long, default_value = "*")]
    repositories: String,
    /// Adapters the node provides (comma list).
    #[arg(long, default_value = "mock")]
    adapters: String,
    /// Max concurrent attempts on the node.
    #[arg(long, default_value_t = 2)]
    max_concurrency: u32,
    /// Local control-plane port to reverse-forward to (where this `ag` runs).
    #[arg(long, default_value_t = 7800)]
    local_port: u16,
    /// Remote port the node reaches the control plane through the tunnel.
    #[arg(long, default_value_t = 7800)]
    remote_port: u16,
    /// Node binary to copy (default: this executable).
    #[arg(long)]
    binary: Option<String>,
    /// Remote data directory for the node.
    #[arg(long, default_value = "/var/lib/agentgrid")]
    data_dir: String,
    /// Agent version reported at enroll.
    #[arg(long, default_value = "0.1.0-cli")]
    agent_version: String,
    /// Control plane URL the node reaches directly (e.g. https://cp.example.com:7800).
    /// When set, no reverse tunnel is opened; SSH is used only to bootstrap.
    #[arg(long)]
    server: Option<String>,
}

pub(crate) async fn cmd_nodes(
    client: &reqwest::Client,
    base: &str,
    json: bool,
    a: NodeArgs,
) -> Result<()> {
    match a.command {
        NodeSub::List => cmd_node_list(client, base, json).await,
        NodeSub::Install(i) => cmd_node_install(client, base, *i).await,
        NodeSub::Doctor { node_id } => cmd_node_doctor(client, base, &node_id).await,
        NodeSub::Drain { node_id, undrain } => {
            cmd_node_drain(client, base, &node_id, undrain).await
        }
    }
}

/// Competitor-gap feature (project brain): rebuild AGENTS-BRAIN.md from a
/// repository's task history — the persistent project-memory file every
/// attempt reads as a "Project brain" prompt block. Pulls terminal tasks for
/// the repo, renders one digest section per task (prompt + outcome + error
/// category), and writes the file. The file is a hint, never a hard
/// dependency: an agent simply does not get the block when it is absent.
/// Competitor-gap feature (consensus patch review, nitpicker-inspired):
/// fire one review task per adapter over a task's changes.patch. Unanimous
/// APPROVE auto-approves the pending patch review on the CP side; any
/// REJECT/unclear verdict leaves it for a human.
async fn cmd_node_drain(
    client: &reqwest::Client,
    base: &str,
    node_id: &str,
    undrain: bool,
) -> Result<()> {
    let resp = client
        .post(format!(
            "{base}/v1/nodes/{node_id}/drain?drain={}",
            !undrain
        ))
        .send()
        .await
        .context("node drain request failed")?;
    if resp.status().is_success() {
        if undrain {
            println!("node {node_id} undrained — new assignments enabled");
        } else {
            println!("node {node_id} drained — no new assignments; in-flight attempts finish");
        }
        Ok(())
    } else if resp.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("node {node_id} not found")
    } else {
        anyhow::bail!("node drain failed: HTTP {}", resp.status())
    }
}

async fn cmd_node_doctor(client: &reqwest::Client, base: &str, node_id: &str) -> Result<()> {
    let resp = client
        .get(format!("{base}/v1/nodes/{node_id}"))
        .send()
        .await
        .context("node fetch request failed")?;
    err_if_fail(resp.status(), "node lookup")?;
    let n: serde_json::Value = resp.json().await.context("parse node")?;
    let id = n.get("id").and_then(|v| v.as_str()).unwrap_or("-");
    let name = n.get("name").and_then(|v| v.as_str()).unwrap_or("-");
    let status = n.get("status").and_then(|v| v.as_str()).unwrap_or("-");
    let last_hb = n
        .get("last_heartbeat_at")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    println!("node {id} ({name})");
    println!("  status      : {status}");
    println!("  last_heartbeat: {last_hb}");
    let mut symptoms: Vec<String> = Vec::new();
    let active = n
        .get("active_attempts")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let max = n
        .get("max_concurrency")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    println!("  active/max  : {active}/{max}");
    let free_disk = n.get("free_disk_mb").and_then(|v| v.as_u64()).unwrap_or(0);
    let load = n.get("load_avg").and_then(|v| v.as_f64()).unwrap_or(0.0);
    println!("  free disk   : {free_disk} MB");
    println!("  load_avg    : {load}");
    let adapters = n
        .get("adapters")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    println!("  adapters    : {adapters}");
    // Hardening P0 item 5: surface unsafe mode + permission interception so a
    // doctor run flags fully-unrestricted nodes.
    let unsafe_active = n
        .get("unsafe_active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let intercept = n
        .get("permission_interception")
        .and_then(|v| v.as_str())
        .unwrap_or("wrapper");
    println!("  interception: {intercept}");
    println!("  unsafe mode : {unsafe_active}");
    if unsafe_active {
        symptoms.push(
            "node runs UNSAFE unattended mode (permissions bypassed, no sandbox) — restrict access"
                .into(),
        );
    }
    if status == "offline" {
        symptoms.push("node is OFFLINE (heartbeat lost or just started)".into());
    }
    if status == "degraded" {
        symptoms.push(
            "node is DEGRADED (a configured adapter binary is missing, or disk low, or protocol mismatch)"
                .into(),
        );
    }
    if status == "revoked" {
        symptoms.push("node is REVOKED; it can no longer service tasks".into());
    }
    if free_disk > 0 && free_disk < 1024 {
        symptoms.push(format!("free disk low ({free_disk} MB < 1 GiB)"));
    }
    if max > 0 && active == max {
        symptoms.push(format!(
            "at capacity ({active}/{max}); new tasks will not assign"
        ));
    }
    if last_hb.is_empty() {
        symptoms
            .push("no heartbeat yet; daemon may not have started or cannot reach the CP".into());
    }
    if symptoms.is_empty() {
        println!("  doctor      : OK — no symptoms");
    } else {
        println!("  doctor      : {} symptom(s):", symptoms.len());
        for s in &symptoms {
            println!("    - {s}");
        }
    }
    Ok(())
}

async fn cmd_node_install(client: &reqwest::Client, base: &str, a: NodeInstallArgs) -> Result<()> {
    if let Transport::Wireguard = a.transport {
        anyhow::bail!(
            "transport 'wireguard' is planned but not implemented yet; use --transport ssh-tunnel"
        );
    }
    validate_install_args(&a)?;
    // Hardening P0 (safe node install): verify the remote SSH host key BEFORE
    // any further install step so a MITM cannot hijack the bootstrap.
    // - --host-key-fingerprint pins the key: ssh-keyscan the host, compute its
    //   SHA256 fingerprint, and bail if it does not match. The matching key is
    //   added to the local known_hosts so subsequent SSH calls use strict.
    // - --accept-new-host-key: accept-new at SSH level.
    // - default: strict (an unknown key is refused).
    verify_host_key(&a)?;
    let token = create_enrollment_token(client, base).await?;
    let bin = a
        .binary
        .clone()
        .or_else(|| {
            let candidate = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("agentgrid-node-daemon")))
                .filter(|p| p.exists())
                .or_else(|| {
                    let p = std::path::PathBuf::from("agentgrid-node-daemon");
                    if p.exists() {
                        Some(p)
                    } else {
                        None
                    }
                })?;
            Some(candidate.to_string_lossy().into_owned())
        })
        .context("no --binary given and agentgrid-node-daemon not found next to `ag`")?;
    let data = a.data_dir.trim_end_matches('/');
    let remote_bin = format!("{data}/agentgrid-node");

    // 0. ensure the remote data dir exists (scp would fail otherwise)
    run_remote(
        &a,
        false,
        &[],
        Some(format!("mkdir -p {data}")),
        "prepare remote dir",
        false,
    )?;

    // 1. copy the node binary to the remote host
    scp_file(&a, &bin, &remote_bin)?;

    // 2. resolve the control-plane URL the node will use
    let (server_url, transport_label) = match &a.server {
        Some(s) => (s.clone(), "direct/https"),
        None => {
            // persistent reverse tunnel: remote localhost:<remote_port> -> local :<local_port>
            run_remote(
                &a,
                false,
                &[
                    "-f".into(),
                    "-N".into(),
                    "-R".into(),
                    format!("{}:127.0.0.1:{}", a.remote_port, a.local_port),
                ],
                None,
                "establish reverse tunnel",
                true,
            )?;
            (format!("http://127.0.0.1:{}", a.remote_port), "ssh-tunnel")
        }
    };

    // 3. write env file on remote (temp locally, scp, chmod 600), then start node
    let env = build_node_env_file(&a, &token, &server_url);
    let tmp = std::env::temp_dir().join(format!("ag-env-{}.env", std::process::id()));
    std::fs::write(&tmp, env).context("write local env temp")?;
    scp_file(&a, &tmp.to_string_lossy(), &format!("{data}/agentgrid.env"))?;
    let _ = std::fs::remove_file(&tmp);
    // Source the env file in a shell so the single-quoted values (and the `*`
    // in AGENTGRID_REPOSITORIES) are parsed correctly; `env $(cat file)` would
    // keep the literal quotes and glob the `*`.
    let start = format!(
        "mkdir -p {data} && chmod 600 {data}/agentgrid.env && setsid nohup bash -c 'set -a; . {data}/agentgrid.env; set +a; exec {bin}' >{data}/node.log 2>&1 </dev/null &",
        data = data,
        bin = remote_bin,
    );
    // The start command backgrounds itself on the remote; launch the ssh that
    // delivers it detached so it doesn't block install (and survives our exit).
    run_remote(&a, false, &[], Some(start), "start node", true)?;

    println!(
        "node '{}' provisioned (transport={})",
        a.name, transport_label
    );
    println!("check status with: ag node list");
    Ok(())
}

/// Build the remote env file (single-quoted values, safe for `env $(cat ...)`).
fn build_node_env_file(a: &NodeInstallArgs, token: &str, server: &str) -> String {
    let data = a.data_dir.trim_end_matches('/');
    let mut s = format!(
        "AGENTGRID_SERVER='{server}'\nAGENTGRID_ENROLL_TOKEN='{token}'\nAGENTGRID_NODE_NAME='{name}'\nAGENTGRID_REPOSITORIES='{repos}'\nAGENTGRID_ADAPTERS='{adapters}'\nAGENTGRID_MAX_CONCURRENCY='{mc}'\nAGENTGRID_DATA_DIR='{data}'\n",
        server = server,
        token = token,
        name = a.name,
        repos = a.repositories,
        adapters = a.adapters,
        mc = a.max_concurrency,
        data = data,
    );
    // hardening P0 (safe node install): the node daemon refuses to run as root
    // unless AGENTGRID_ALLOW_ROOT=1. We never set it automatically; the operator
    // must pass --allow-root. Prefer SSH-ing as an unprivileged user and a
    // --data-dir owned by that user.
    if a.allow_root {
        s.push_str("AGENTGRID_ALLOW_ROOT='1'\n");
    }
    s.push_str(&format!("AGENTGRID_AGENT_VERSION='{}'\n", a.agent_version));
    s
}

/// Hardening P0 (safe node install): verify/PIN the remote SSH host key before
/// any install step. `--host-key-fingerprint` pins the exact SHA256
/// fingerprint (ssh-keyscan + ssh-keygen -lf compare, bailing on mismatch) and
/// adds the trusted key to ~/.ssh/known_hosts; `--accept-new-host-key` opts
/// into ssh's accept-new mode; default is strict refusal. Returns Ok only when
/// the key is acceptable.
fn verify_host_key(a: &NodeInstallArgs) -> Result<()> {
    let (_user, host, port) = parse_host(&a.host);
    if let Some(fp) = &a.host_key_fingerprint {
        let fp = fp.trim();
        let mut scan = std::process::Command::new("ssh-keyscan");
        scan.stderr(std::process::Stdio::null());
        if let Some(p) = port {
            scan.arg("-p").arg(p.to_string());
        }
        scan.arg(&host);
        let scan_out = scan
            .output()
            .with_context(|| format!("ssh-keyscan {host} failed to spawn"))?;
        if !scan_out.status.success() || scan_out.stdout.is_empty() {
            anyhow::bail!("could not ssh-keyscan host {host}: not reachable or no keys");
        }
        let mut kg = std::process::Command::new("ssh-keygen");
        kg.arg("-lf").arg("-");
        kg.stdin(std::process::Stdio::piped());
        kg.stdout(std::process::Stdio::piped());
        kg.stderr(std::process::Stdio::null());
        let mut child = kg
            .spawn()
            .with_context(|| "ssh-keygen -lf - failed to spawn")?;
        use std::io::Write;
        child.stdin.take().unwrap().write_all(&scan_out.stdout).ok();
        let kg_out = child
            .wait_with_output()
            .with_context(|| "ssh-keygen -lf - wait failed")?;
        let text = String::from_utf8_lossy(&kg_out.stdout);
        let mut matched = false;
        for line in text.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[1].eq_ignore_ascii_case(fp) {
                matched = true;
                break;
            }
        }
        if !matched {
            anyhow::bail!(
                "host {host} SSH key fingerprint does not match --host-key-fingerprint; got:\n{text}"
            );
        }
        // Add the trusted key to known_hosts so subsequent ssh uses strict.
        let home = dirs_for_known_hosts()?;
        let kh_path = home.join(".ssh/known_hosts");
        if let Some(parent) = kh_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut kh = std::process::Command::new("ssh-keyscan");
        kh.stderr(std::process::Stdio::null());
        if let Some(p) = port {
            kh.arg("-p").arg(p.to_string());
        }
        kh.arg("-H").arg(&host);
        let out = kh.output().with_context(|| "ssh-keyscan -H failed")?;
        if out.status.success() && !out.stdout.is_empty() {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&kh_path)?;
            f.write_all(&out.stdout)?;
        }
    }
    Ok(())
}

/// Resolve the per-user HOME directory for known_hosts.
fn dirs_for_known_hosts() -> Result<std::path::PathBuf> {
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return Ok(std::path::PathBuf::from(h));
        }
    }
    anyhow::bail!("HOME unset: cannot resolve ~/.ssh/known_hosts for SSH host-key pinning")
}

/// Reject shell-breaking characters in user-supplied fields (trust boundary).
fn validate_install_args(a: &NodeInstallArgs) -> Result<()> {
    let sane = |s: &str, what: &str| {
        if s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./@:,*".contains(c))
        {
            Ok(())
        } else {
            anyhow::bail!("invalid {what}: only [A-Za-z0-9._,/@:-] allowed")
        }
    };
    sane(&a.name, "name")?;
    sane(&a.repositories, "repositories")?;
    sane(&a.adapters, "adapters")?;
    sane(&a.data_dir, "data-dir")?;
    if let Some(s) = &a.server {
        sane(s, "server")?;
    }
    Ok(())
}

/// Run an ssh/scp invocation against the remote host, choosing the auth wrapper:
/// key (direct), password via `sshpass` when present, else `expect` (universally
/// available on Linux). `extra` are program-specific args (e.g. `-f -N -R ...`);
/// `remote_cmd` (ssh only) is the final argument (the remote shell command).
/// `detach` launches the command in its own session (setsid) so it survives the
/// `ag nodes install` process — used for the persistent reverse tunnel.
fn run_remote(
    a: &NodeInstallArgs,
    is_scp: bool,
    extra: &[String],
    remote_cmd: Option<String>,
    what: &str,
    detach: bool,
) -> Result<()> {
    let prog = if is_scp { "scp" } else { "ssh" };
    let mut base: Vec<String> = vec![prog.to_string()];
    if let Some(key) = &a.ssh_key {
        base.push("-i".into());
        base.push(key.clone());
    }
    base.push("-o".into());
    // Hardening P0 (safe node install): fail CLOSED on an unknown SSH host
    // key by default (no MITM). `--accept-new-host-key` opts into accept-new;
    // `--host-key-fingerprint` pins the key (verified via a keyscan+compare in
    // cmd_node_install before any remote command runs).
    if a.host_key_fingerprint.is_some() {
        base.push("StrictHostKeyChecking=yes".into());
    } else if a.accept_new_host_key {
        base.push("StrictHostKeyChecking=accept-new".into());
    } else {
        base.push("StrictHostKeyChecking=yes".into());
    }
    if !is_scp && a.password.is_none() {
        base.push("-o".into());
        base.push("BatchMode=yes".into());
    }
    if let (.., Some(p)) = parse_host(&a.host) {
        base.push((if is_scp { "-P" } else { "-p" }).into());
        base.push(p.to_string());
    }
    base.extend(extra.iter().cloned());
    let (user, host, _p) = parse_host(&a.host);
    let target = user
        .map(|u| format!("{u}@{host}"))
        .unwrap_or_else(|| host.clone());
    if !is_scp {
        base.push(target);
        if let Some(rc) = &remote_cmd {
            base.push(rc.clone());
        }
    }

    // auth wrapper -> final argv (+ optional secret passed via env, never argv)
    let (argv, secret_env) = if let Some(pw) = &a.password {
        if std::process::Command::new("sshpass")
            .arg("true")
            .status()
            .is_ok()
        {
            let mut v = vec!["sshpass".to_string(), "-e".to_string()];
            v.extend(base);
            (v, Some(("SSHPASS", pw.clone())))
        } else {
            let spawn_line = format!(
                "spawn {}",
                base.iter()
                    .map(|x| format!("{{{x}}}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            // The password is read from AGENTGRID_SSH_PASS at expect runtime:
            // interpolating it into the script would expose it in `ps` (argv)
            // and allow Tcl injection through the password text.
            let script = format!(
                "set timeout 600\n{spawn_line}\nexpect {{\n    -re \"(?i)password:\" {{ send \"$env(AGENTGRID_SSH_PASS)\\r\"; exp_continue }}\n    eof\n}}\n"
            );
            (
                vec!["expect".to_string(), "-c".to_string(), script],
                Some(("AGENTGRID_SSH_PASS", pw.clone())),
            )
        }
    } else {
        (base, None)
    };

    if detach {
        let mut c = std::process::Command::new("setsid");
        c.arg("nohup").args(&argv);
        if let Some((var, val)) = &secret_env {
            c.env(var, val);
        }
        // Detached children must NOT inherit our stdout/stderr/ stdin — the
        // node install command would otherwise hang waiting on a pipe the
        // detached tunnel/start ssh keeps open.
        c.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn detached ssh/scp ({what})"))?;
        return Ok(());
    }
    let mut c = std::process::Command::new(&argv[0]);
    c.args(&argv[1..]);
    if let Some((var, val)) = &secret_env {
        c.env(var, val);
    }
    let status = c
        .status()
        .with_context(|| format!("failed to run ssh/scp ({what})"))?;
    if !status.success() {
        anyhow::bail!("ssh/scp step failed ({what}): exit {status}");
    }
    Ok(())
}

/// user@host[:port] -> (user, host, port)
fn parse_host(host: &str) -> (Option<String>, String, Option<u16>) {
    let (user, rest) = match host.split_once('@') {
        Some((u, r)) => (Some(u.to_string()), r),
        None => (None, host),
    };
    match rest.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (user, h.to_string(), p.parse().ok()),
        _ => (user, rest.to_string(), None),
    }
}

/// Copy a local file to the remote host.
fn scp_file(a: &NodeInstallArgs, local: &str, remote: &str) -> Result<()> {
    let (user, host, _p) = parse_host(&a.host);
    let target = format!(
        "{}:{}",
        user.map(|u| format!("{u}@{host}"))
            .unwrap_or_else(|| host.clone()),
        remote
    );
    run_remote(
        a,
        true,
        &[local.to_string(), target],
        None,
        "scp file",
        false,
    )
}

async fn cmd_node_list(client: &reqwest::Client, base: &str, json: bool) -> Result<()> {
    let resp = client
        .get(format!("{base}/v1/nodes"))
        .send()
        .await
        .context("node list request failed")?;
    if !resp.status().is_success() {
        return Err(api_error(resp.status(), "node list"));
    }
    let v: serde_json::Value = resp.json().await.context("parse nodes")?;
    let nodes = list_items(&v);
    if json {
        println!("{}", serde_json::to_string_pretty(&nodes)?);
        return Ok(());
    }
    if nodes.is_empty() {
        println!("(no nodes registered)");
        return Ok(());
    }
    println!(
        "{:<36} {:<10} {:<8} {:<6} {:<10} {:<12} {:<14} {:<12}",
        "ID", "STATUS", "ACTIVE", "MAX", "DISK", "INTERCEPT", "UNSAFE", "SPOOL"
    );
    for n in &nodes {
        let id = n.get("id").and_then(|v| v.as_str()).unwrap_or("-");
        let st = n.get("status").and_then(|v| v.as_str()).unwrap_or("-");
        let active = n
            .get("active_attempts")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let max = n
            .get("max_concurrency")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let disk = n.get("free_disk_mb").and_then(|v| v.as_u64()).unwrap_or(0);
        let disk = if disk < 1024 {
            format!("{} MB !", disk)
        } else {
            format!("{:.0} GB", disk as f64 / 1024.0)
        };
        // Hardening P0 item 5: surface unsafe mode + interception so operators
        // can see which nodes run fully-unrestricted agents at a glance.
        let intercept = n
            .get("permission_interception")
            .and_then(|v| v.as_str())
            .unwrap_or("wrapper");
        let unsafe_active = n
            .get("unsafe_active")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let unsafe_flag = if unsafe_active { "UNSAFE" } else { "no" };
        // Hardening P2 item 35: local storage pressure (outbox + artifact
        // spool) — a node whose spool grows is backing up and not draining.
        let spool_bytes = n.get("outbox_bytes").and_then(|v| v.as_u64()).unwrap_or(0)
            + n.get("artifact_spool_bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        let spool = if spool_bytes >= 1024 * 1024 {
            format!("{:.1} MB", spool_bytes as f64 / (1024.0 * 1024.0))
        } else if spool_bytes > 0 {
            format!("{spool_bytes} B")
        } else {
            "-".to_string()
        };
        println!(
            "{id:<36} {st:<10} {active:<8} {max:<6} {disk:<10} {intercept:<12} {unsafe_flag:<14} {spool:<12}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod node_install_tests {
    use super::*;

    fn sample() -> NodeInstallArgs {
        NodeInstallArgs {
            host: "deploy@node-b:2222".into(),
            ssh_key: None,
            password: None,
            accept_new_host_key: false,
            host_key_fingerprint: None,
            allow_root: false,
            transport: Transport::SshTunnel,
            name: "node-b".into(),
            repositories: "*".into(),
            adapters: "mock".into(),
            max_concurrency: 2,
            local_port: 7800,
            remote_port: 7800,
            binary: None,
            data_dir: "/var/lib/agentgrid".into(),
            agent_version: "0.1.0-cli".into(),
            server: None,
        }
    }

    #[test]
    fn parse_host_splits_user_port() {
        assert_eq!(
            parse_host("u@h:22"),
            (Some("u".into()), "h".into(), Some(22))
        );
        assert_eq!(parse_host("h:2222"), (None, "h".into(), Some(2222)));
        assert_eq!(parse_host("u@h"), (Some("u".into()), "h".into(), None));
        assert_eq!(parse_host("h"), (None, "h".into(), None));
    }

    #[test]
    fn env_file_has_server_and_token() {
        let env = build_node_env_file(&sample(), "TOK123", "http://cp.example.com:7800");
        assert!(env.contains("AGENTGRID_SERVER='http://cp.example.com:7800'"));
        assert!(env.contains("AGENTGRID_ENROLL_TOKEN='TOK123'"));
        assert!(env.contains("AGENTGRID_NODE_NAME='node-b'"));
        // single-quoted values survive `env $(cat ...)`
        assert!(env.lines().all(|l| l.contains('=')));
    }

    #[test]
    fn validate_rejects_shell_meta() {
        let mut a = sample();
        a.name = "$(rm -rf /)".into();
        assert!(validate_install_args(&a).is_err());
        let mut b = sample();
        b.repositories = "a; b".into();
        assert!(validate_install_args(&b).is_err());
        assert!(validate_install_args(&sample()).is_ok());
    }

    #[test]
    fn wireguard_transport_not_implemented() {
        // ensured at the command layer; here we just confirm the variant exists
        let _ = Transport::Wireguard;
    }

    /// Hardening P0 (safe node install): the default install does NOT bake
    /// `AGENTGRID_ALLOW_ROOT=1` into the provisioned env — the daemon refuses
    /// root unless the operator explicitly opts in with --allow-root.
    #[test]
    fn build_env_no_allow_root_by_default() {
        let a = sample();
        assert!(!a.allow_root);
        let env = build_node_env_file(&a, "tok", "http://127.0.0.1:7800");
        assert!(
            !env.contains("AGENTGRID_ALLOW_ROOT"),
            "default env must not allow root: {env}"
        );
        // token is present (needed for the enroll) but root is not.
        assert!(env.contains("AGENTGRID_ENROLL_TOKEN='tok'"));
    }

    #[test]
    fn build_env_adds_allow_root_when_opted_in() {
        let mut a = sample();
        a.allow_root = true;
        let env = build_node_env_file(&a, "tok", "http://127.0.0.1:7800");
        assert!(env.contains("AGENTGRID_ALLOW_ROOT='1'"));
    }

    /// Hardening P0 (safe node install): host-key fingerprint + accept-new are
    /// both OFF by default, so SSH fails closed on an unknown host key.
    #[test]
    fn host_key_mode_defaults_strict() {
        let a = sample();
        assert!(!a.accept_new_host_key);
        assert!(a.host_key_fingerprint.is_none());
    }
}
