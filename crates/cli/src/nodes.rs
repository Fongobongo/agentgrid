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
    /// Print the one-line command that adds a host to this control plane.
    ///
    /// `install` pushes from here over SSH and needs credentials *to* the host.
    /// This is the other direction: the host fetches what it needs from the
    /// control plane it is joining, so the only requirement is that the host
    /// can reach something — with `--via`, a sibling node's relay, for a host
    /// that cannot reach the control plane at all.
    Add {
        /// Node name the daemon registers under. The install record is matched
        /// by it when the host arrives.
        #[arg(long)]
        name: String,
        /// user@host or user@host:port. Recorded so the install list says which
        /// machine this is; nothing dials it.
        #[arg(long)]
        host: Option<String>,
        /// Node id or name whose relay carries the join. Use it when the new
        /// host cannot reach this control plane directly: `ag nodes ls` shows
        /// which nodes announce a relay in the RELAY column.
        #[arg(long)]
        via: Option<String>,
    },
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
    /// Issue a one-time re-key token for an existing node. The node keeps its
    /// id, its task history and the key its adapter-env secrets are sealed to;
    /// only its credential is replaced. This is the recovery path for a leaked
    /// `credential.json` — revoking and re-enrolling would give the node a new
    /// id and drop every sealed secret with it.
    Rekey { node_id: String },
    /// Delete a node-install record: the trace of a host that was configured
    /// and will never arrive.
    ///
    /// Those records are kept indefinitely on purpose — they answer "was this
    /// host ever configured?", which is the one question the node list cannot
    /// answer, because a host whose daemon never enrolled has no node row at
    /// all. Removing one is therefore an explicit act. This deletes the install
    /// attempt only; a node that did enroll is a different object, and its
    /// history is revoked, not deleted.
    Forget { install_id: String },
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
        NodeSub::Add { name, host, via } => {
            cmd_node_add(client, base, json, &name, host.as_deref(), via.as_deref()).await
        }
        NodeSub::Install(i) => cmd_node_install(client, base, *i).await,
        NodeSub::Doctor { node_id } => cmd_node_doctor(client, base, &node_id).await,
        NodeSub::Drain { node_id, undrain } => {
            cmd_node_drain(client, base, &node_id, undrain).await
        }
        NodeSub::Rekey { node_id } => cmd_node_rekey(client, base, json, &node_id).await,
        NodeSub::Forget { install_id } => cmd_node_forget(client, base, &install_id).await,
    }
}

/// Ask the control plane for the one line that joins a new host.
///
/// The command is built server-side rather than formatted here: only the
/// control plane knows which nodes announced a relay address, and `--via` is
/// resolved against that list. Formatting it locally would mean the CLI picked
/// a relay from a list it had to fetch anyway, with less information.
async fn cmd_node_add(
    client: &reqwest::Client,
    base: &str,
    json: bool,
    name: &str,
    host: Option<&str>,
    via: Option<&str>,
) -> Result<()> {
    let mut body = serde_json::json!({ "name": name });
    if let Some(h) = host.map(str::trim).filter(|h| !h.is_empty()) {
        body["host"] = serde_json::json!(h);
    }
    if let Some(v) = via.map(str::trim).filter(|v| !v.is_empty()) {
        body["via"] = serde_json::json!(v);
    }
    let resp = client
        .post(format!("{base}/v1/nodes/join-command"))
        .json(&body)
        .send()
        .await
        .context("join-command request failed")?;
    // 409 means "that node cannot relay" and 404 "no such node"; both carry the
    // server's own explanation, which is better than anything reconstructed
    // here from a status code.
    if !resp.status().is_success() {
        let status = resp.status();
        let detail = resp.text().await.unwrap_or_default();
        let detail = detail.trim();
        anyhow::bail!(
            "could not build a join command (HTTP {status}){}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }
    let out: serde_json::Value = resp.json().await.context("parse join command")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    let command = out
        .get("command")
        .and_then(|v| v.as_str())
        .context("join-command response missing 'command'")?;
    let expires = out
        .get("expires_at")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    println!("Run this on the new host ({name}), as root. The token is single-use");
    println!("and expires at {expires}:");
    println!();
    println!("  {command}");
    println!();
    match out.get("via").and_then(|v| v.as_object()) {
        Some(via) => println!(
            "It dials {} ({}) — this control plane is not reachable from that host.",
            via.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
            via.get("relay_url").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        None => println!(
            "It dials this control plane ({}). If the host cannot reach it, re-run with",
            out.get("server").and_then(|v| v.as_str()).unwrap_or(base)
        ),
    }
    if out.get("via").is_none() {
        println!("--via <node> — `ag nodes ls` shows which nodes can carry the traffic.");
    }
    // A revoked relay is refused by the control plane, so anything else here is
    // a warning rather than a refusal: an offline relay may be back by the time
    // the operator pastes the line. Saying nothing would be the worse choice —
    // the command is minted for a host nobody can log into.
    if let Some(via) = out.get("via") {
        let status = via.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if !status.is_empty() && status != "online" {
            println!();
            println!(
                "Warning: that relay is {status} — last heartbeat {}.",
                via.get("last_heartbeat_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("never")
            );
            println!("Add --probe to the line above to stop before installing anything if");
            println!("the new host cannot reach it.");
        }
    }
    println!();
    println!("The host appears in `ag nodes ls` once it enrolls; until then it is an");
    println!("`issued` row in the install list.");
    Ok(())
}

/// Delete one install record (a host that was configured and will never
/// arrive). 404 is reported as "not found" rather than a silent success: a
/// typo must not read as "removed".
async fn cmd_node_forget(client: &reqwest::Client, base: &str, install_id: &str) -> Result<()> {
    let resp = client
        .delete(format!("{base}/v1/node-installs/{install_id}"))
        .send()
        .await
        .context("node-install forget request failed")?;
    err_if_fail(resp.status(), "node-install forget")?;
    println!("forgot install record {install_id}");
    Ok(())
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

async fn cmd_node_rekey(
    client: &reqwest::Client,
    base: &str,
    json: bool,
    node_id: &str,
) -> Result<()> {
    let resp = client
        .post(format!("{base}/v1/nodes/{node_id}/rekey-token"))
        .send()
        .await
        .context("re-key token request failed")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("node {node_id} not found");
    }
    err_if_fail(resp.status(), "re-key token")?;
    let body: serde_json::Value = resp.json().await?;
    let token = body
        .get("token")
        .and_then(|v| v.as_str())
        .context("re-key token response missing 'token'")?;
    let expires = body
        .get("expires_at")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    // `--json` for scripts (the re-key E2E reads the token out of it); the
    // human form is the same token plus the command that spends it.
    if json {
        println!(
            "{}",
            serde_json::json!({ "node_id": node_id, "token": token, "expires_at": expires })
        );
        return Ok(());
    }
    println!("re-key token for node {node_id} (valid until {expires}):");
    println!();
    println!("  {token}");
    println!();
    println!("On the node host, restart the daemon once with both variables set:");
    println!();
    println!("  AGENTGRID_REKEY=1 AGENTGRID_ENROLL_TOKEN={token} \\");
    println!("    <the daemon's usual command>");
    println!();
    println!("The node keeps its id, its task history and its sealing key, so every");
    println!("adapter-env secret sealed for it stays readable. The token is single-use");
    println!("and the daemon scrubs it (and the flag) from AGENTGRID_ENV_FILE after use.");
    Ok(())
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
    let target = a.target();
    let plan = a.plan(binary);
    // Name the host at mint time: this install runs from here, not from the
    // control plane, so the token is the only thing the CP will ever hear about
    // it. Without this, a host that was installed and never came up had no
    // record anywhere.
    let token = create_enrollment_token(client, base, Some(&target.host), Some(&plan.name)).await?;
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
        "{:<36} {:<10} {:<8} {:<6} {:<10} {:<12} {:<14} {:<12} {:<12} {:<22} {:<10}",
        "ID",
        "STATUS",
        "ACTIVE",
        "MAX",
        "DISK",
        "INTERCEPT",
        "UNSAFE",
        "SPOOL",
        "REACH",
        "RELAY",
        "REPOS"
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
        let reach = reach_cell(n);
        let relay = relay_cell(n);
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
            "{id:<36} {st:<10} {active:<8} {max:<6} {disk:<10} {intercept:<12} {unsafe_flag:<14} {spool:<12} {reach:<12} {relay:<22} {repo_cell:<10}"
        );
    }
    Ok(())
}

/// Does this node carry other nodes' traffic, and where do they dial it?
///
/// This is the announcement a host that cannot reach the control plane is
/// joined through (`ag nodes add --via`), so it is the answer to "which of my
/// nodes can I use as a stepping stone" without opening the panel. `-` is "not
/// a relay"; the scheme is always http and is dropped to keep the column
/// readable.
fn relay_cell(n: &serde_json::Value) -> String {
    let url = n
        .get("relay_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if !url.is_empty() {
        return url
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .to_string();
    }
    // Listening but announcing nothing is not the same as not being a relay,
    // and it used to render as the same `-`. It is a relay that will never be
    // used until AGENTGRID_RELAY_ADVERTISE says where to dial it.
    let listen = n
        .get("relay_listen")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if listen.is_empty() {
        return "-".to_string();
    }
    let port = listen.rsplit(':').next().unwrap_or(listen);
    format!(":{port} (no advertise)")
}

/// ADR 0026 + ADR 0012: how this node reaches the control plane.
///
/// `direct` is only printed when both facts are known and neither says
/// otherwise — `?` means no heartbeat has carried either column yet, and
/// claiming `direct` for that would be a guess. `relay×N` is what the control
/// plane observed on the request; `proxy` is what the node claims.
fn reach_cell(n: &serde_json::Value) -> String {
    let hops = n.get("relay_hops").and_then(|v| v.as_u64());
    let proxy = n.get("egress_proxy").and_then(|v| v.as_bool());
    if hops.is_none() && proxy.is_none() {
        return "?".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(h) = hops {
        if h > 0 {
            parts.push(format!("relay×{h}"));
        }
    }
    if proxy == Some(true) {
        parts.push("proxy".to_string());
    }
    if parts.is_empty() {
        "direct".to_string()
    } else {
        parts.join("+")
    }
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

#[cfg(test)]
mod reach_cell_tests {
    use super::reach_cell;
    use serde_json::json;

    /// ADR 0026 / ADR 0012: `ag nodes ls` answers "is this node connected
    /// directly?" without opening the panel.
    #[test]
    fn reach_cell_names_the_path_or_admits_it_does_not_know() {
        // Never reported: `?`, never `direct` — that would be a guess.
        assert_eq!(reach_cell(&json!({})), "?");
        assert_eq!(
            reach_cell(&json!({ "relay_hops": null, "egress_proxy": null })),
            "?"
        );

        // Observed 0 hops and no proxy claim: direct.
        assert_eq!(
            reach_cell(&json!({ "relay_hops": 0, "egress_proxy": false })),
            "direct"
        );
        // A node on an older binary sends no `egress_proxy`; the observation
        // alone is enough to call it direct.
        assert_eq!(reach_cell(&json!({ "relay_hops": 0 })), "direct");

        assert_eq!(reach_cell(&json!({ "relay_hops": 2 })), "relay×2");
        assert_eq!(
            reach_cell(&json!({ "relay_hops": 0, "egress_proxy": true })),
            "proxy"
        );
        // Both at once: the two facts are independent, so neither may hide the
        // other.
        assert_eq!(
            reach_cell(&json!({ "relay_hops": 1, "egress_proxy": true })),
            "relay×1+proxy"
        );
    }
}

#[cfg(test)]
mod relay_cell_tests {
    use super::relay_cell;
    use serde_json::json;

    /// `ag nodes ls` is how an operator finds a stepping stone for a host that
    /// cannot reach the control plane, so the RELAY column has to say "not a
    /// relay" differently from "a relay with no dialable address" — the second
    /// one looks configured and is useless.
    #[test]
    fn relay_cell_shows_the_dialable_address_only_when_there_is_one() {
        assert_eq!(relay_cell(&json!({})), "-");
        assert_eq!(relay_cell(&json!({ "relay_url": null })), "-");
        assert_eq!(relay_cell(&json!({ "relay_url": "" })), "-");
        assert_eq!(
            relay_cell(&json!({ "relay_url": "http://node-a.internal:7900" })),
            "node-a.internal:7900",
            "the scheme is the same for every relay; the address is the information"
        );
        assert_eq!(
            relay_cell(&json!({ "relay_url": "https://relay.example.com" })),
            "relay.example.com"
        );
    }

    /// A relay that listens and announces nothing is running and useless, and
    /// it used to render as `-` — the same as a node that is not a relay at
    /// all. The operator then went looking for a variable they had already set.
    #[test]
    fn a_relay_that_announces_nothing_is_not_shown_as_no_relay() {
        assert_eq!(
            relay_cell(&json!({ "relay_listen": "0.0.0.0:7900" })),
            ":7900 (no advertise)"
        );
        // A dialable address wins: the listen address is not what anyone dials.
        assert_eq!(
            relay_cell(&json!({
                "relay_url": "http://node-a.internal:7900",
                "relay_listen": "0.0.0.0:7900",
            })),
            "node-a.internal:7900"
        );
        // Neither field is still "not a relay".
        assert_eq!(
            relay_cell(&json!({ "relay_url": null, "relay_listen": null })),
            "-"
        );
    }
}
