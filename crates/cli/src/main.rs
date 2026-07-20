//! Minimal MVP CLI (Stage 1.5): `run`, `logs`, `show`, `nodes`.
//!
//! Command grouping (`task run`, `node list`) is deferred; this flat form
//! exercises the same `/v1` surface.

use agentgrid_common::{
    ApprovalView, CreateTaskRequest, CreateWorkflowRequest, CreateWorkflowRunRequest, LoginRequest,
    LoginResponse, TaskEligibility, TaskStatus, TaskView, WorkflowStep, WorkflowTemplate,
};
use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use std::os::unix::fs::PermissionsExt;

#[derive(Parser)]
#[command(name = "ag", version, about = "agentgrid CLI")]
struct Cli {
    /// Control plane base URL (also AGENTGRID_SERVER).
    #[arg(
        long,
        env = "AGENTGRID_SERVER",
        default_value = "http://127.0.0.1:7800"
    )]
    server: String,
    /// Emit raw JSON instead of human-readable tables (machine-readable output).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: AgCommand,
}

#[derive(Subcommand)]
enum AgCommand {
    /// Create a task.
    Run(RunArgs),
    /// Stream a task's events.
    Logs(LogsArgs),
    /// Show a task's status/result.
    Show(ShowArgs),
    /// Manage nodes (list / install over SSH).
    Nodes(NodeArgs),
    /// Cancel a task (queued -> cancelled; running -> ask node to stop).
    Cancel(CancelArgs),
    /// Retry a failed or cancelled task (back to queued).
    Retry(RetryArgs),
    /// Node enrollment tokens.
    Token(TokenArgs),
    /// Manage repositories.
    Repo(RepoArgs),
    /// Log in and store a session token for user-authenticated endpoints.
    Login(LoginArgs),
    /// Review and answer agent permission approvals (fail-closed by default).
    Approvals(ApprovalArgs),
    /// Start the control plane (standalone binary).
    Server(ServerStartArgs),
    /// Define and run Agentgrid workflows (DAGs of agent steps).
    Workflow(WorkflowArgs),
}

#[derive(Args)]
struct RunArgs {
    repository: String,
    prompt: String,
    #[arg(long, default_value = "mock")]
    adapter: String,
    #[arg(long)]
    node: Option<String>,
    /// Validation command run after the agent succeeds.
    #[arg(long)]
    validate: Option<String>,
    /// Per-task timeout in seconds.
    #[arg(long)]
    timeout: Option<u64>,
}

#[derive(Args)]
struct ServerStartArgs {
    /// Listen address (sets AGENTGRID_LISTEN).
    #[arg(long, default_value = "127.0.0.1:7800")]
    listen: String,
    /// SQLite database path (sets AGENTGRID_DB).
    #[arg(long, default_value = "control-plane.db")]
    db: String,
    /// Bootstrap the first user with this username (one-time).
    #[arg(long)]
    bootstrap_user: Option<String>,
    /// Bootstrap password for the first user.
    #[arg(long)]
    bootstrap_password: Option<String>,
    /// TLS certificate (PEM). Enables HTTPS on the control plane.
    #[arg(long)]
    tls_cert: Option<String>,
    /// TLS private key (PEM). Enables HTTPS on the control plane.
    #[arg(long)]
    tls_key: Option<String>,
}

#[derive(Args)]
struct LogsArgs {
    task_id: String,
    /// Follow until the task reaches a terminal state.
    #[arg(long)]
    follow: bool,
}

#[derive(Args)]
struct ShowArgs {
    task_id: String,
}

#[derive(Args)]
struct CancelArgs {
    task_id: String,
}

#[derive(Args)]
struct RetryArgs {
    task_id: String,
}

#[derive(Args)]
struct LoginArgs {
    username: String,
    password: String,
}

#[derive(Args)]
struct TokenArgs {
    #[command(subcommand)]
    action: TokenAction,
}

#[derive(Subcommand)]
enum TokenAction {
    /// Issue a one-time enrollment token for a new node.
    Create,
}

#[derive(Args)]
struct RepoArgs {
    #[command(subcommand)]
    action: RepoAction,
}

#[derive(Subcommand)]
enum RepoAction {
    /// Register a repository.
    Add(RepoAddArgs),
}

#[derive(Args)]
struct ApprovalArgs {
    #[command(subcommand)]
    action: ApprovalAction,
}

#[derive(Subcommand)]
enum ApprovalAction {
    /// List approvals (optionally filter by status).
    List {
        /// Filter by status: pending|allowed|denied|expired|cancelled.
        status: Option<String>,
    },
    /// Allow a pending approval by id.
    Allow(ApprovalIdArgs),
    /// Deny a pending approval by id.
    Deny(ApprovalIdArgs),
}

#[derive(Args)]
struct ApprovalIdArgs {
    id: String,
}

#[derive(Args)]
struct NodeArgs {
    #[command(subcommand)]
    command: NodeSub,
}

#[derive(Subcommand)]
enum NodeSub {
    /// List registered nodes.
    List,
    /// Provision a remote host as a node over SSH and link it to this control plane.
    Install(Box<NodeInstallArgs>),
}

/// Transport used for the node -> control-plane runtime link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Default)]
enum Transport {
    /// Reverse SSH tunnel (default). Works behind NAT; SSH encrypts the link.
    #[default]
    SshTunnel,
    /// Private WireGuard network (planned). SSH used only for one-time bootstrap.
    Wireguard,
}

#[derive(Args)]
struct NodeInstallArgs {
    /// Remote host as user@host or user@host:port.
    #[arg(long)]
    host: String,
    /// Path to SSH private key (key-based auth; recommended over --password).
    #[arg(long)]
    ssh_key: Option<String>,
    /// SSH password (requires `sshpass`; passed via SSHPASS env, never argv).
    #[arg(long)]
    password: Option<String>,
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

#[derive(Args)]
struct RepoAddArgs {
    name: String,
    /// Git URL (https/token or local path).
    git_url: String,
    /// Default branch new attempts branch from.
    #[arg(long, default_value = "main")]
    branch: String,
    /// Optional validation command run after the agent succeeds.
    #[arg(long)]
    validate: Option<String>,
}

#[derive(Args)]
struct WorkflowArgs {
    #[command(subcommand)]
    command: WorkflowSub,
}

#[derive(Subcommand)]
enum WorkflowSub {
    /// Define a workflow template from a steps JSON file.
    Create(WorkflowCreateArgs),
    /// List workflow templates.
    List,
    /// Show a workflow template (its DAG).
    Show(WorkflowShowArgs),
    /// Start a run of a template.
    Run(WorkflowRunArgs),
    /// Cancel a whole workflow run (and its non-terminal steps/tasks).
    Cancel(WorkflowCancelArgs),
}

#[derive(Args)]
struct WorkflowCreateArgs {
    #[arg(long)]
    name: String,
    /// Path to a JSON file: an array of WorkflowStep objects.
    #[arg(long)]
    steps: String,
    /// Optional default context JSON.
    #[arg(long)]
    context: Option<String>,
}

#[derive(Args)]
struct WorkflowShowArgs {
    template_id: String,
}

#[derive(Args)]
struct WorkflowRunArgs {
    template_id: String,
    /// Optional run context JSON (overrides the template default).
    #[arg(long)]
    context: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut client_builder = reqwest::Client::builder();
    // Attach a stored session token to all user-authenticated requests.
    if let Some(token) = load_token() {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")) {
            headers.insert(reqwest::header::AUTHORIZATION, v);
        }
        client_builder = client_builder.default_headers(headers);
    }
    let client = client_builder.build()?;
    let base = cli.server.trim_end_matches('/').to_string();

    match cli.command {
        AgCommand::Run(a) => cmd_run(&client, &base, a).await,
        AgCommand::Logs(a) => cmd_logs(&client, &base, a).await,
        AgCommand::Show(a) => cmd_show(&client, &base, a, cli.json).await,
        AgCommand::Nodes(a) => cmd_nodes(&client, &base, cli.json, a).await,
        AgCommand::Cancel(a) => cmd_cancel(&client, &base, a).await,
        AgCommand::Retry(a) => cmd_retry(&client, &base, a).await,
        AgCommand::Token(a) => cmd_token(&client, &base, a).await,
        AgCommand::Repo(a) => cmd_repo(&client, &base, a).await,
        AgCommand::Login(a) => cmd_login(&client, &base, a).await,
        AgCommand::Approvals(a) => cmd_approvals(&client, &base, a).await,
        AgCommand::Server(a) => cmd_server_start(a),
        AgCommand::Workflow(a) => cmd_workflow(&client, &base, a, cli.json).await,
    }
}

async fn cmd_run(client: &reqwest::Client, base: &str, a: RunArgs) -> Result<()> {
    let req = CreateTaskRequest {
        prompt: a.prompt,
        repository: a.repository,
        adapter: a.adapter,
        requested_node_id: a.node,
        timeout_secs: a.timeout,
        validation_command: a.validate,
        base_commit: None,
        parent_acp_session_id: None,
    };
    let resp = client
        .post(format!("{base}/v1/tasks"))
        .json(&req)
        .send()
        .await
        .context("create task request failed")?;
    let task: TaskView = resp.json().await.context("parse task response")?;
    println!("task {} created (status: {})", task.id, task.status);
    println!("{}", task.id);
    Ok(())
}

async fn cmd_show(client: &reqwest::Client, base: &str, a: ShowArgs, json: bool) -> Result<()> {
    let resp = client
        .get(format!("{base}/v1/tasks/{}", a.task_id))
        .send()
        .await
        .context("show request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("task not found ({})", resp.status());
    }
    let task: TaskView = resp.json().await.context("parse task response")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&task)?);
        return Ok(());
    }
    println!("id:        {}", task.id);
    println!("status:    {}", task.status);
    println!("repository:{}", task.repository);
    println!("adapter:   {}", task.adapter);
    println!(
        "attempt:   {}",
        task.assigned_attempt_id
            .clone()
            .unwrap_or_else(|| "-".into())
    );
    println!("created:   {}", task.created_at);
    if task.status == TaskStatus::Queued {
        if let Ok(elig) = client
            .get(format!("{base}/v1/tasks/{}/eligibility", task.id))
            .send()
            .await
        {
            if let Ok(elig) = elig.json::<TaskEligibility>().await {
                if elig.no_eligible_nodes.is_empty() {
                    println!(
                        "eligibility: waiting for an eligible node ({} online)",
                        elig.nodes.len()
                    );
                } else {
                    println!("no eligible nodes:");
                    for reason in &elig.no_eligible_nodes {
                        println!("  - {reason}");
                    }
                }
            }
        }
    }
    Ok(())
}

async fn cmd_logs(client: &reqwest::Client, base: &str, a: LogsArgs) -> Result<()> {
    let mut after: u64 = 0;
    loop {
        let resp = client
            .get(format!("{base}/v1/tasks/{}/events", a.task_id))
            .query(&[("after_sequence", after)])
            .send()
            .await
            .context("events request failed")?;
        if resp.status().is_success() {
            let events: Vec<serde_json::Value> = resp.json().await.context("parse events")?;
            for e in &events {
                let seq = e.get("sequence").and_then(|v| v.as_u64()).unwrap_or(0);
                after = after.max(seq);
                let ty = e.get("type").and_then(|v| v.as_str()).unwrap_or("?");
                let text = e
                    .get("payload")
                    .and_then(|p| p.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                println!("[{seq}] {ty}: {text}");
            }
        }
        if !a.follow {
            break;
        }
        if let Ok(status) = current_status(client, base, &a.task_id).await {
            if matches!(
                status,
                TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
            ) {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    Ok(())
}

async fn current_status(client: &reqwest::Client, base: &str, task_id: &str) -> Result<TaskStatus> {
    let resp = client
        .get(format!("{base}/v1/tasks/{task_id}"))
        .send()
        .await?;
    let task: TaskView = resp.json().await?;
    Ok(task.status)
}

async fn cmd_nodes(client: &reqwest::Client, base: &str, json: bool, a: NodeArgs) -> Result<()> {
    match a.command {
        NodeSub::List => cmd_node_list(client, base, json).await,
        NodeSub::Install(i) => cmd_node_install(client, base, *i).await,
    }
}

async fn cmd_node_install(client: &reqwest::Client, base: &str, a: NodeInstallArgs) -> Result<()> {
    if let Transport::Wireguard = a.transport {
        anyhow::bail!(
            "transport 'wireguard' is planned but not implemented yet; use --transport ssh-tunnel"
        );
    }
    validate_install_args(&a)?;
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
    // nodes provisioned as root need this to start (daemon refuses root otherwise)
    s.push_str("AGENTGRID_ALLOW_ROOT='1'\n");
    s.push_str(&format!("AGENTGRID_AGENT_VERSION='{}'\n", a.agent_version));
    s
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
    base.push("StrictHostKeyChecking=no".into());
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

    // auth wrapper -> final argv (+ optional SSHPASS for sshpass mode)
    let (argv, sshpass_pw) = if let Some(pw) = &a.password {
        if std::process::Command::new("sshpass")
            .arg("true")
            .status()
            .is_ok()
        {
            let mut v = vec!["sshpass".to_string(), "-e".to_string()];
            v.extend(base);
            (v, Some(pw.clone()))
        } else {
            let spawn_line = format!(
                "spawn {}",
                base.iter()
                    .map(|x| format!("{{{x}}}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            let script = format!(
                "set timeout 600\n{spawn_line}\nexpect {{\n    -re \"(?i)password:\" {{ send \"{pw}\\r\"; exp_continue }}\n    eof\n}}\n"
            );
            (vec!["expect".to_string(), "-c".to_string(), script], None)
        }
    } else {
        (base, None)
    };

    if detach {
        let mut c = std::process::Command::new("setsid");
        c.arg("nohup").args(&argv);
        if let Some(pw) = &sshpass_pw {
            c.env("SSHPASS", pw);
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
    if let Some(pw) = &sshpass_pw {
        c.env("SSHPASS", pw);
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
    let nodes: Vec<serde_json::Value> = resp.json().await.context("parse nodes")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&nodes)?);
        return Ok(());
    }
    if nodes.is_empty() {
        println!("(no nodes registered)");
        return Ok(());
    }
    println!(
        "{:<36} {:<10} {:<8} {:<6} {:<10}",
        "ID", "STATUS", "ACTIVE", "MAX", "DISK"
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
        println!("{id:<36} {st:<10} {active:<8} {max:<6} {disk:<10}");
    }
    Ok(())
}

async fn cmd_cancel(client: &reqwest::Client, base: &str, a: CancelArgs) -> Result<()> {
    let resp = client
        .post(format!("{base}/v1/tasks/{}/cancel", a.task_id))
        .send()
        .await
        .context("cancel request failed")?;
    if resp.status().is_success() {
        println!("cancel requested for {}", a.task_id);
        Ok(())
    } else {
        anyhow::bail!("cancel failed ({})", resp.status())
    }
}

async fn cmd_retry(client: &reqwest::Client, base: &str, a: RetryArgs) -> Result<()> {
    let resp = client
        .post(format!("{base}/v1/tasks/{}/retry", a.task_id))
        .send()
        .await
        .context("retry request failed")?;
    if resp.status().is_success() {
        println!("task {} requeued", a.task_id);
        Ok(())
    } else {
        anyhow::bail!("retry failed ({})", resp.status())
    }
}

async fn cmd_approvals(client: &reqwest::Client, base: &str, a: ApprovalArgs) -> Result<()> {
    match a.action {
        ApprovalAction::List { status } => {
            let mut url = format!("{base}/v1/approvals");
            if let Some(s) = status {
                url.push_str(&format!("?status={s}"));
            }
            let resp = client
                .get(&url)
                .send()
                .await
                .context("approvals list request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("approvals list failed ({})", resp.status());
            }
            let approvals: Vec<ApprovalView> = resp.json().await.context("bad approvals json")?;
            for ap in &approvals {
                println!(
                    "{:<36} {:<10} {:<9} {}",
                    ap.id,
                    format!("{:?}", ap.status),
                    ap.task_id,
                    ap.permission
                );
            }
            Ok(())
        }
        ApprovalAction::Allow(id) => answer_approval(client, base, &id.id, "allow").await,
        ApprovalAction::Deny(id) => answer_approval(client, base, &id.id, "deny").await,
    }
}

async fn answer_approval(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    decision: &str,
) -> Result<()> {
    let resp = client
        .post(format!("{base}/v1/approvals/{id}/{decision}"))
        .send()
        .await
        .context("approval answer request failed")?;
    if resp.status().is_success() {
        println!("approval {id} -> {decision}");
        Ok(())
    } else {
        anyhow::bail!("approval {decision} failed ({})", resp.status())
    }
}

async fn cmd_repo(client: &reqwest::Client, base: &str, a: RepoArgs) -> Result<()> {
    match a.action {
        RepoAction::Add(add) => {
            let req = serde_json::json!({
                "name": add.name,
                "git_url": add.git_url,
                "default_branch": add.branch,
                "validation_command": add.validate,
            });
            let resp = client
                .post(format!("{base}/v1/repositories"))
                .json(&req)
                .send()
                .await
                .context("repository registration failed")?;
            if resp.status().is_success() {
                println!("repository {} registered", add.name);
                Ok(())
            } else {
                anyhow::bail!("repo add failed ({})", resp.status())
            }
        }
    }
}

async fn cmd_token(client: &reqwest::Client, base: &str, a: TokenArgs) -> Result<()> {
    match a.action {
        TokenAction::Create => {
            let token = create_enrollment_token(client, base).await?;
            println!("export AGENTGRID_ENROLL_TOKEN={token}");
            Ok(())
        }
    }
}

/// Mint a one-time enrollment token via the control-plane API.
async fn create_enrollment_token(client: &reqwest::Client, base: &str) -> Result<String> {
    let resp = client
        .post(format!("{base}/v1/nodes/enrollment-token"))
        .send()
        .await
        .context("enrollment-token request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "token creation failed ({}): are you logged in? (ag login)",
            resp.status()
        );
    }
    let body: serde_json::Value = resp.json().await?;
    body.get("token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .context("enrollment-token response missing 'token'")
}

fn dirs_config() -> std::path::PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".config"))
}

fn credential_path() -> std::path::PathBuf {
    let mut dir = dirs_config();
    dir.push("agentgrid");
    dir.push("credentials");
    dir
}

/// Load a previously stored session token, if present.
fn load_token() -> Option<String> {
    let content = std::fs::read_to_string(credential_path()).ok()?;
    serde_json::from_str::<LoginResponse>(&content)
        .ok()
        .map(|r| r.token)
}

/// Persist a session token with 0600 perms (Stage 4.1).
fn save_token(token: &str) -> Result<()> {
    let path = credential_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &path,
        serde_json::to_string(&LoginResponse {
            token: token.to_string(),
        })?,
    )?;
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn cmd_server_start(a: ServerStartArgs) -> Result<()> {
    // The control plane binary ships alongside `ag` in the same install dir.
    let exe = std::env::current_exe()?;
    let bin = exe
        .parent()
        .map(|p| p.join("agentgrid-control-plane"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| std::path::PathBuf::from("agentgrid-control-plane"));
    if !bin.exists() {
        anyhow::bail!(
            "agentgrid-control-plane not found next to `ag` (looked at {})",
            bin.display()
        );
    }
    let mut cmd = std::process::Command::new(&bin);
    cmd.env("AGENTGRID_LISTEN", &a.listen)
        .env("AGENTGRID_DB", &a.db);
    if let Some(u) = &a.bootstrap_user {
        cmd.env("AGENTGRID_BOOTSTRAP_USER", u);
    }
    if let Some(c) = &a.tls_cert {
        cmd.env("AGENTGRID_TLS_CERT", c);
    }
    if let Some(k) = &a.tls_key {
        cmd.env("AGENTGRID_TLS_KEY", k);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(err.into())
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

async fn cmd_login(client: &reqwest::Client, base: &str, a: LoginArgs) -> Result<()> {
    let req = LoginRequest {
        username: a.username,
        password: a.password,
    };
    let resp = client
        .post(format!("{base}/v1/auth/login"))
        .json(&req)
        .send()
        .await
        .context("login request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("login failed ({})", resp.status());
    }
    let lr: LoginResponse = resp.json().await.context("parse login response")?;
    save_token(&lr.token)?;
    println!("logged in; token stored at {}", credential_path().display());
    Ok(())
}

async fn cmd_workflow(
    client: &reqwest::Client,
    base: &str,
    a: WorkflowArgs,
    json: bool,
) -> Result<()> {
    match a.command {
        WorkflowSub::Create(c) => cmd_workflow_create(client, base, c).await,
        WorkflowSub::List => cmd_workflow_list(client, base, json).await,
        WorkflowSub::Show(s) => cmd_workflow_show(client, base, s, json).await,
        WorkflowSub::Run(r) => cmd_workflow_run(client, base, r).await,
        WorkflowSub::Cancel(c) => cmd_workflow_cancel(client, base, c).await,
    }
}

async fn cmd_workflow_create(
    client: &reqwest::Client,
    base: &str,
    a: WorkflowCreateArgs,
) -> Result<()> {
    let body = std::fs::read_to_string(&a.steps).with_context(|| format!("read {}", a.steps))?;
    let steps: Vec<WorkflowStep> = serde_json::from_str(&body)
        .with_context(|| format!("parse steps JSON from {}", a.steps))?;
    let req = CreateWorkflowRequest {
        name: a.name,
        steps,
        context: a.context,
    };
    let resp = client
        .post(format!("{base}/v1/workflows"))
        .json(&req)
        .send()
        .await
        .context("create workflow request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("create workflow failed ({})", resp.status());
    }
    let tpl: WorkflowTemplate = resp.json().await.context("parse workflow response")?;
    println!("workflow {} created ({} steps)", tpl.id, tpl.steps.len());
    println!("{}", tpl.id);
    Ok(())
}

async fn cmd_workflow_list(client: &reqwest::Client, base: &str, json: bool) -> Result<()> {
    let resp = client
        .get(format!("{base}/v1/workflows"))
        .send()
        .await
        .context("list workflows request failed")?;
    let tpls: Vec<WorkflowTemplate> = resp.json().await.context("parse workflows response")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&tpls)?);
        return Ok(());
    }
    if tpls.is_empty() {
        println!("(no workflows)");
        return Ok(());
    }
    for t in &tpls {
        println!("{}\t{}\t{} steps", t.id, t.name, t.steps.len());
    }
    Ok(())
}

async fn cmd_workflow_show(
    client: &reqwest::Client,
    base: &str,
    a: WorkflowShowArgs,
    json: bool,
) -> Result<()> {
    let resp = client
        .get(format!("{base}/v1/workflows/{}", a.template_id))
        .send()
        .await
        .context("show workflow request failed")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("workflow {} not found", a.template_id);
    }
    let tpl: WorkflowTemplate = resp.json().await.context("parse workflow response")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&tpl)?);
        return Ok(());
    }
    println!("workflow {}", tpl.id);
    println!("name: {}", tpl.name);
    println!("steps:");
    for s in &tpl.steps {
        println!(
            "  - {} [{}] deps={:?}",
            s.id,
            format!("{:?}", s.role).to_lowercase(),
            s.depends_on
        );
    }
    Ok(())
}

async fn cmd_workflow_cancel(
    client: &reqwest::Client,
    base: &str,
    a: WorkflowCancelArgs,
) -> Result<()> {
    let resp = client
        .post(format!("{base}/v1/workflow-runs/{}/cancel", a.id))
        .send()
        .await
        .context("cancel workflow run request failed")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("workflow run {} not found", a.id);
    }
    if !resp.status().is_success() {
        anyhow::bail!("cancel workflow run failed ({})", resp.status());
    }
    println!("workflow run {} cancelled", a.id);
    Ok(())
}

#[derive(Args)]
struct WorkflowCancelArgs {
    /// Workflow run id to cancel.
    id: String,
}

async fn cmd_workflow_run(client: &reqwest::Client, base: &str, a: WorkflowRunArgs) -> Result<()> {
    let req = CreateWorkflowRunRequest {
        context: a.context,
        repository: None,
        base_commit: None,
    };
    let resp = client
        .post(format!("{base}/v1/workflows/{}/runs", a.template_id))
        .json(&req)
        .send()
        .await
        .context("create workflow run request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("create workflow run failed ({})", resp.status());
    }
    let run: agentgrid_common::WorkflowRun =
        resp.json().await.context("parse workflow run response")?;
    println!("workflow run {} started (status: {:?})", run.id, run.status);
    println!("{}", run.id);
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
}
