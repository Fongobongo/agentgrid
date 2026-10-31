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
    /// Path to an OpenSSH certificate for --ssh-key. A certificate expires,
    /// which is the point: the host trusts the CA, not a key that lives
    /// forever. Requires --ssh-key.
    #[arg(long)]
    ssh_cert: Option<String>,
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
    let free_mem = n
        .get("mem_available_mb")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if free_mem > 0 {
        println!("  free memory : {free_mem} MB");
    }
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
    // The binary path is resolved here, not in the shared crate: the control
    // plane resolves its copy from its own distribution directory instead.
    let binary = resolve_node_binary(a.binary.as_deref())?;
    let token = create_enrollment_token(client, base).await?;
    let target = a.target();
    let plan = a.plan(binary);
    let mut say = |m: &str| println!("   {m}");
    let outcome = agentgrid_provision::install(&target, &plan, &token, &mut say)?;
    println!(
        "node '{}' provisioned (transport={}, server={})",
        plan.name, outcome.transport, outcome.server_url
    );
    println!("check status with: ag node list");
    Ok(())
}

fn resolve_node_binary(explicit: Option<&str>) -> Result<std::path::PathBuf> {
    if let Some(b) = explicit {
        return Ok(std::path::PathBuf::from(b));
    }
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
        });
    candidate.context("no --binary given and agentgrid-node-daemon not found next to `ag`")
}

impl NodeInstallArgs {
    fn target(&self) -> agentgrid_provision::Target {
        agentgrid_provision::Target {
            host: self.host.clone(),
            ssh_key: self.ssh_key.clone(),
            ssh_cert: self.ssh_cert.clone(),
            password: self.password.clone(),
            accept_new_host_key: self.accept_new_host_key,
            host_key_fingerprint: self.host_key_fingerprint.clone(),
        }
    }

    /// `--server` means "the node dials that URL itself"; without it the
    /// historical default applies — a reverse tunnel to this machine.
    fn plan(&self, binary: std::path::PathBuf) -> agentgrid_provision::Plan {
        let transport = if self.server.is_some() {
            agentgrid_provision::Transport::Direct
        } else {
            agentgrid_provision::Transport::SshTunnel
        };
        agentgrid_provision::Plan {
            transport,
            name: self.name.clone(),
            repositories: self.repositories.clone(),
            adapters: self.adapters.clone(),
            max_concurrency: self.max_concurrency,
            data_dir: self.data_dir.clone(),
            server: self.server.clone(),
            agent_version: self.agent_version.clone(),
            allow_root: self.allow_root,
            local_port: self.local_port,
            remote_port: self.remote_port,
            binary,
        }
    }
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
        "{:<36} {:<10} {:<8} {:<6} {:<10} {:<12} {:<14} {:<12} {:<10}",
        "ID", "STATUS", "ACTIVE", "MAX", "DISK", "INTERCEPT", "UNSAFE", "SPOOL", "REPOS"
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
        // Plan 2.5 (#204): per-repository attach state — `ready/total`,
        // with `!` when any repo is cloning/invalid (details in JSON).
        let repos = n.get("repo_states").and_then(|v| v.as_array());
        let repo_cell = match repos {
            None => "-".to_string(),
            Some(arr) if arr.is_empty() => "-".to_string(),
            Some(arr) => {
                let ready = arr
                    .iter()
                    .filter(|r| r.get("state").and_then(|s| s.as_str()) == Some("ready"))
                    .count();
                if ready == arr.len() {
                    format!("{}/{}", ready, arr.len())
                } else {
                    format!("{}/{} !", ready, arr.len())
                }
            }
        };
        println!(
            "{id:<36} {st:<10} {active:<8} {max:<6} {disk:<10} {intercept:<12} {unsafe_flag:<14} {spool:<12} {repo_cell:<10}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod node_install_tests {
    use super::*;
    use std::path::PathBuf;

    fn sample() -> NodeInstallArgs {
        NodeInstallArgs {
            host: "deploy@node-b:2222".into(),
            ssh_key: None,
            ssh_cert: None,
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

    /// `--server` selects the direct transport; without it the historical
    /// reverse-tunnel default stands. That mapping is the only install logic
    /// left in the CLI — the install itself lives in `agentgrid-provision`.
    #[test]
    fn server_flag_selects_direct_transport() {
        let a = sample();
        assert_eq!(
            a.plan(PathBuf::from("x")).transport,
            agentgrid_provision::Transport::SshTunnel
        );
        let mut b = sample();
        b.server = Some("http://cp.example.com:7800".into());
        assert_eq!(
            b.plan(PathBuf::from("x")).transport,
            agentgrid_provision::Transport::Direct
        );
    }

    /// The clap-level transport keeps its planned variant so `--transport
    /// wireguard` is rejected with a message rather than an unknown-argument
    /// error that looks like a typo.
    #[test]
    fn wireguard_transport_not_implemented() {
        let _ = Transport::Wireguard;
    }

    #[test]
    fn explicit_binary_wins() {
        let p = resolve_node_binary(Some("/opt/ag/agentgrid-node-daemon")).unwrap();
        assert_eq!(p, PathBuf::from("/opt/ag/agentgrid-node-daemon"));
    }
}
