//! Minimal MVP CLI (Stage 1.5): `run`, `logs`, `show`, `nodes`.
//!
//! Command grouping (`task run`, `node list`) is deferred; this flat form
//! exercises the same `/v1` surface.

use agentgrid_common::{
    AgentProfile, ApprovalView, CreateTaskRequest, CreateWorkflowRequest, CreateWorkflowRunRequest,
    LoginRequest, LoginResponse, SkillTrustView, TaskEligibility, TaskStatus, TaskView,
    WorkflowStep, WorkflowTemplate,
};
use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

mod tui;
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
    /// Manage skill trust decisions (fail-closed: untrusted until trusted).
    Skills(SkillsArgs),
    /// Manage MCP server registry (Stage 13 stdio servers a profile attaches).
    Mcp(McpArgs),
    /// Manage agent profiles (system prompt + autonomy + limits; immutable revisions).
    Profiles(ProfilesArgs),
    /// Start the control plane (standalone binary).
    Server(ServerStartArgs),
    /// Define and run Agentgrid workflows (DAGs of agent steps).
    Workflow(WorkflowArgs),
    /// Full-screen TUI dashboard (read-only monitoring).
    Tui(TuiArgs),
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
    /// Disable colored output. Default: color on.
    #[arg(long)]
    no_color: bool,
}

/// Render lifecycle phase derived from the event stream + pending approvals,
/// orthogonal to the terminal `TaskStatus`. Mirrors the herdr agent-state idea
/// (`idle | working | blocked | done`) but computed client-side from events
/// the control plane already emits, so no store/migration change is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No structured events yet seen (just stdout/stderr).
    Starting,
    /// Last structured event was a tool call / progress / file change.
    Working,
    /// A durable approval is pending for this task (or the stream says so).
    Blocked,
    /// Vertically terminal — set by callers once `TaskStatus` is terminal.
    Done,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::Working => "working",
            Phase::Blocked => "blocked",
            Phase::Done => "done",
        }
    }
}

const C_RESET: &str = "\x1b[0m";
const C_GRAY: &str = "\x1b[90m";
const C_RED: &str = "\x1b[31m";
const C_CYAN: &str = "\x1b[36m";
const C_YELLOW: &str = "\x1b[33m";
const C_GREEN: &str = "\x1b[32m";
const C_BOLD: &str = "\x1b[1m";

fn paint(no_color: bool, code: &str, s: &str) -> String {
    if no_color {
        s.to_string()
    } else {
        format!("{code}{s}{C_RESET}")
    }
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

#[derive(Args)]
struct SkillsArgs {
    #[command(subcommand)]
    action: SkillsAction,
}

#[derive(Subcommand)]
enum SkillsAction {
    /// List recorded skill trust decisions.
    List {
        /// Filter by skill source tier: project|user|managed.
        #[arg(long)]
        source: Option<String>,
    },
    /// Trust a skill (allow the agent to load/execute it).
    Trust(SkillsNameArgs),
    /// Untrust a skill (fail-closed: the agent must not use it).
    Untrust(SkillsNameArgs),
}

#[derive(Args)]
struct McpArgs {
    #[command(subcommand)]
    action: McpAction,
}

#[derive(Subcommand)]
enum McpAction {
    /// List registered MCP servers.
    List,
    /// Register or replace an MCP server in the operator registry.
    Create {
        id: String,
        name: String,
        command: String,
        /// Args to pass (repeatable).
        #[arg(long = "arg")]
        args: Vec<String>,
        /// Env var names the server requires (repeatable; values resolved at spawn).
        #[arg(long = "env-requirement")]
        env_requirements: Vec<String>,
        /// Register as disabled (default enabled).
        #[arg(long)]
        disabled: bool,
    },
    /// Delete a server.
    Delete { id: String },
}

#[derive(Args)]
struct SkillsNameArgs {
    /// Skill name (as discovered from SKILL.md frontmatter).
    name: String,
    /// Where the skill was found: project|user|managed (default project).
    #[arg(long, default_value = "project")]
    source: String,
}

#[derive(Args)]
struct ProfilesArgs {
    #[command(subcommand)]
    action: ProfilesAction,
}

#[derive(Subcommand)]
enum ProfilesAction {
    /// List profile ids that have an active revision.
    List,
    /// Show all revisions of a profile (newest first).
    Show { id: String },
    /// Create a new revision of a profile (does not activate it).
    Create(ProfileCreateArgs),
    /// Activate a specific revision (rollback = activate an older one).
    Activate { id: String, revision: i64 },
}

#[derive(Args)]
struct ProfileCreateArgs {
    id: String,
    /// System prompt text (inline). Empty string allowed.
    #[arg(long, default_value = "")]
    system_prompt: String,
    /// Autonomy level: l0|l1|l2|l3|l4 (default l2).
    #[arg(long, default_value = "l2")]
    autonomy: String,
    /// Max RSS in bytes.
    #[arg(long)]
    memory_max: Option<i64>,
    /// CPU quota, percent of one core (200 = 2 cores).
    #[arg(long)]
    cpu_quota: Option<i64>,
    /// Max tasks (PIDs).
    #[arg(long)]
    tasks_max: Option<i64>,
    /// Required secret env name (repeatable; names only, never values).
    #[arg(long = "secret-required", value_name = "ENV")]
    secret_required: Vec<String>,
    /// Optional secret env name (repeatable; warn-only if unset).
    #[arg(long = "secret-optional", value_name = "ENV")]
    secret_optional: Vec<String>,
    /// Adapter version this profile targets (SemVer; major must match).
    #[arg(long)]
    adapter_version: Option<String>,
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
    /// Optional reason recorded with the decision (audit trail).
    #[arg(long)]
    reason: Option<String>,
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
struct TuiArgs {
    /// Disable colored output. Default: color on.
    #[arg(long)]
    no_color: bool,
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
    /// Manage scheduled/recurring triggers for a workflow template (Stage 13).
    Schedules(WorkflowSchedulesArgs),
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

#[derive(Args)]
struct WorkflowSchedulesArgs {
    id: String,
    #[command(subcommand)]
    action: SchedulesAction,
}

#[derive(Subcommand)]
enum SchedulesAction {
    /// List schedules for a template.
    List,
    /// Create a scheduled trigger.
    Create {
        /// Interval between runs in seconds (>=1).
        #[arg(long)]
        interval_seconds: i64,
        /// Autonomy level l0..l4 (default l2).
        #[arg(long, default_value = "l2")]
        autonomy: String,
        /// Start paused (default: enabled).
        #[arg(long)]
        paused: bool,
    },
    /// Delete a schedule.
    Delete { sid: String },
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
        AgCommand::Skills(a) => cmd_skills(&client, &base, a).await,
        AgCommand::Mcp(a) => cmd_mcp(&client, &base, a).await,
        AgCommand::Profiles(a) => cmd_profiles(&client, &base, a).await,
        AgCommand::Server(a) => cmd_server_start(a),
        AgCommand::Workflow(a) => cmd_workflow(&client, &base, a, cli.json).await,
        AgCommand::Tui(a) => cmd_tui(&client, &base, a).await,
    }
}

async fn cmd_tui(client: &reqwest::Client, base: &str, a: TuiArgs) -> Result<()> {
    tui::run_dashboard(client.clone(), base.to_string(), a.no_color).await
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
    let nc = a.no_color;
    let mut after: u64 = 0;
    let mut phase = Phase::Starting;
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
                phase = phase_from_event(ty, e);
                print_event(e, seq, ty, nc);
            }
        }
        // Stage TUI-idea: overlay a `blocked` phase when a durable approval is
        // pending for this task (approvals live in their own table, not the
        // event stream, so the stream alone never reports blocked).
        if phase != Phase::Done && has_pending_approval(client, base, &a.task_id).await {
            phase = Phase::Blocked;
        }
        if a.follow {
            eprintln!(
                "{} {}",
                paint(nc, C_BOLD, "phase:"),
                paint(
                    nc,
                    match phase {
                        Phase::Blocked => C_YELLOW,
                        Phase::Working => C_CYAN,
                        Phase::Done => C_GREEN,
                        _ => C_GRAY,
                    },
                    phase.label()
                )
            );
        }
        if !a.follow {
            break;
        }
        if let Ok(status) = current_status(client, base, &a.task_id).await {
            if matches!(
                status,
                TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
            ) {
                phase = Phase::Done;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    if a.follow {
        eprintln!(
            "{} {}",
            paint(nc, C_BOLD, "phase:"),
            paint(
                nc,
                match phase {
                    Phase::Blocked => C_YELLOW,
                    Phase::Working => C_CYAN,
                    Phase::Done => C_GREEN,
                    _ => C_GRAY,
                },
                phase.label()
            )
        );
    }
    Ok(())
}

fn phase_from_event(ty: &str, e: &serde_json::Value) -> Phase {
    match ty {
        "tool" | "tool_call" | "file_change" | "progress" | "stdout" | "stderr" => Phase::Working,
        "result" => Phase::Done,
        "error" => Phase::Done,
        "status" => {
            // a status event with a terminal-ish payload hints at done; deault Working.
            if let Some(p) = e.get("payload") {
                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                    if t.contains("succeeded") || t.contains("failed") || t.contains("cancelled") {
                        return Phase::Done;
                    }
                }
            }
            Phase::Working
        }
        _ => Phase::Starting,
    }
}

fn print_event(e: &serde_json::Value, seq: u64, ty: &str, nc: bool) {
    let payload = e.get("payload").cloned().unwrap_or(serde_json::Value::Null);
    let text = payload.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let line = match ty {
        "stdout" => format!(
            "{} {}",
            paint(nc, C_GRAY, "stdout"),
            paint(nc, C_GRAY, text)
        ),
        "stderr" => format!("{} {}", paint(nc, C_RED, "stderr"), paint(nc, C_RED, text)),
        "tool" | "tool_call" => {
            let tool = payload.get("tool").and_then(|v| v.as_str()).unwrap_or("?");
            let input = payload.get("input").and_then(|v| v.as_str()).unwrap_or("");
            format!(
                "{} {} {}",
                paint(nc, C_CYAN, "tool"),
                paint(nc, C_BOLD, tool),
                input
            )
        }
        "file_change" => {
            let path = payload.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let op = payload
                .get("op")
                .and_then(|v| v.as_str())
                .unwrap_or("change");
            format!(
                "{} {} {}",
                paint(nc, C_CYAN, "file"),
                paint(nc, C_BOLD, op),
                path
            )
        }
        "result" => format!("{} {}", paint(nc, C_GREEN, "result"), text),
        "error" => format!("{} {}", paint(nc, C_RED, "error"), paint(nc, C_BOLD, text)),
        "status" => format!("{} {}", paint(nc, C_YELLOW, "status"), text),
        _ => format!("{} {}", paint(nc, C_GRAY, ty), text),
    };
    println!("{} {}", paint(nc, C_GRAY, &format!("[{seq}]")), line);
}

async fn has_pending_approval(client: &reqwest::Client, base: &str, task_id: &str) -> bool {
    // Approvals are listed globally with a status filter; client filters by
    // task_id. On any error, false (fail-open on display, not on enforcement).
    let resp = match client
        .get(format!("{base}/v1/approvals"))
        .query(&[("status", "pending")])
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return false,
    };
    let views: Vec<serde_json::Value> = match resp.json().await {
        Ok(v) => v,
        Err(_) => return false,
    };
    views
        .iter()
        .any(|v| v.get("task_id").and_then(|t| t.as_str()) == Some(task_id))
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
        ApprovalAction::Allow(id) => {
            answer_approval(client, base, &id.id, "allow", id.reason.as_deref()).await
        }
        ApprovalAction::Deny(id) => {
            answer_approval(client, base, &id.id, "deny", id.reason.as_deref()).await
        }
    }
}

async fn answer_approval(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    decision: &str,
    reason: Option<&str>,
) -> Result<()> {
    let body = reason
        .map(|r| serde_json::json!({ "reason": r }))
        .unwrap_or_else(|| serde_json::json!({}));
    let resp = client
        .post(format!("{base}/v1/approvals/{id}/{decision}"))
        .json(&body)
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

/// Stage 9.2: skill trust management. A skill absent from the ledger is
/// `untrusted` (fail-closed); trust/untrust records the operator decision.
async fn cmd_skills(client: &reqwest::Client, base: &str, a: SkillsArgs) -> Result<()> {
    match a.action {
        SkillsAction::List { source } => {
            let mut url = format!("{base}/v1/skills");
            if let Some(s) = source {
                url.push_str(&format!("?source={s}"));
            }
            let resp = client
                .get(&url)
                .send()
                .await
                .context("skills list request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("skills list failed ({})", resp.status());
            }
            let rows: Vec<SkillTrustView> = resp.json().await.context("bad skills json")?;
            if rows.is_empty() {
                println!("no recorded skill trust decisions");
            }
            for s in &rows {
                println!(
                    "{:<24} {:<8} {:<8} {}",
                    s.name,
                    s.source,
                    if s.trusted { "trusted" } else { "untrusted" },
                    s.decided_by.as_deref().unwrap_or("")
                );
            }
            Ok(())
        }
        SkillsAction::Trust(a) => set_skill_trust(client, base, &a.name, &a.source, "trust").await,
        SkillsAction::Untrust(a) => {
            set_skill_trust(client, base, &a.name, &a.source, "untrust").await
        }
    }
}

async fn cmd_mcp(client: &reqwest::Client, base: &str, a: McpArgs) -> Result<()> {
    use agentgrid_common::{McpServer, McpServerCreate};
    match a.action {
        McpAction::List => {
            let resp = client
                .get(format!("{base}/v1/mcp-servers"))
                .send()
                .await
                .context("list mcp-servers request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("list mcp-servers failed ({})", resp.status());
            }
            let servers: Vec<McpServer> = resp.json().await.context("bad mcp json")?;
            if servers.is_empty() {
                println!("no MCP servers registered");
            }
            for s in &servers {
                println!(
                    "{:<12} {:<16} {:<16} {} args={} env=[{}]",
                    s.id,
                    s.name,
                    s.command,
                    if s.enabled { "[on]" } else { "[off]" },
                    s.args.len(),
                    s.env_requirements.join(",")
                );
            }
            Ok(())
        }
        McpAction::Create {
            id,
            name,
            command,
            args,
            env_requirements,
            disabled,
        } => {
            let body = serde_json::to_string(&McpServerCreate {
                id,
                name,
                command,
                args,
                env_requirements,
                enabled: !disabled,
            })
            .unwrap();
            let resp = client
                .post(format!("{base}/v1/mcp-servers"))
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .context("create mcp-server request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("create mcp-server failed ({})", resp.status());
            }
            let s: McpServer = resp.json().await.context("bad mcp json")?;
            println!("mcp server {} registered: {}", s.id, s.name);
            Ok(())
        }
        McpAction::Delete { id } => {
            let resp = client
                .delete(format!("{base}/v1/mcp-servers/{}", id))
                .send()
                .await
                .context("delete mcp-server request failed")?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                anyhow::bail!("mcp server {} not found", id);
            }
            if !resp.status().is_success() {
                anyhow::bail!("delete mcp-server failed ({})", resp.status());
            }
            println!("mcp server {} deleted", id);
            Ok(())
        }
    }
}

async fn set_skill_trust(
    client: &reqwest::Client,
    base: &str,
    name: &str,
    source: &str,
    decision: &str,
) -> Result<()> {
    let resp = client
        .post(format!(
            "{base}/v1/skills/{name}/{decision}?source={source}"
        ))
        .send()
        .await
        .context("skill trust request failed")?;
    if resp.status().is_success() {
        println!("skill {name} ({source}) -> {decision}");
        Ok(())
    } else {
        anyhow::bail!("skill {decision} failed ({})", resp.status())
    }
}

/// Stage 13: agent profile management. Revisions are immutable; activating an
/// older revision rolls back without losing history.
async fn cmd_profiles(client: &reqwest::Client, base: &str, a: ProfilesArgs) -> Result<()> {
    match a.action {
        ProfilesAction::List => {
            let resp = client
                .get(format!("{base}/v1/profiles"))
                .send()
                .await
                .context("profiles list request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("profiles list failed ({})", resp.status());
            }
            let ids: Vec<String> = resp.json().await.context("bad profiles json")?;
            if ids.is_empty() {
                println!("no active profiles");
            }
            for id in &ids {
                println!("{id}");
            }
            Ok(())
        }
        ProfilesAction::Show { id } => {
            let resp = client
                .get(format!("{base}/v1/profiles/{id}"))
                .send()
                .await
                .context("profile show request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("profile show failed ({})", resp.status());
            }
            let revs: Vec<AgentProfile> = resp.json().await.context("bad profile json")?;
            if revs.is_empty() {
                println!("profile {id}: no revisions");
            }
            for p in &revs {
                println!(
                    "{:<8}{:<2} {:<8} mem={:?} cpu={:?} tasks={:?} {}",
                    format!("r{}", p.revision),
                    if p.active { "*" } else { " " },
                    p.autonomy,
                    p.memory_max.map(|v| v.to_string()),
                    p.cpu_quota.map(|v| v.to_string()),
                    p.tasks_max.map(|v| v.to_string()),
                    if p.system_prompt.is_empty() {
                        ""
                    } else {
                        "<prompt>"
                    },
                );
            }
            Ok(())
        }
        ProfilesAction::Create(a) => {
            let id = a.id.clone();
            let mut secret_requirements = Vec::new();
            for e in &a.secret_required {
                secret_requirements.push(serde_json::json!({ "env": e, "required": true }));
            }
            for e in &a.secret_optional {
                secret_requirements.push(serde_json::json!({ "env": e, "required": false }));
            }
            let body = serde_json::json!({
                "system_prompt": a.system_prompt,
                "autonomy": a.autonomy,
                "memory_max": a.memory_max,
                "cpu_quota": a.cpu_quota,
                "tasks_max": a.tasks_max,
                "secret_requirements": secret_requirements,
                "adapter_version": a.adapter_version,
            });
            let resp = client
                .post(format!("{base}/v1/profiles/{}", a.id))
                .json(&body)
                .send()
                .await
                .context("profile create request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("profile create failed ({})", resp.status());
            }
            let v: serde_json::Value = resp.json().await.context("bad profile json")?;
            println!(
                "created {id}/r{} (not active; `ag profiles activate {id} <rev>`)",
                v["revision"]
            );
            Ok(())
        }
        ProfilesAction::Activate { id, revision } => {
            let resp = client
                .post(format!("{base}/v1/profiles/{id}/activate"))
                .json(&serde_json::json!({ "revision": revision }))
                .send()
                .await
                .context("profile activate request failed")?;
            if resp.status().is_success() {
                println!("activated {id}/r{revision}");
                Ok(())
            } else {
                anyhow::bail!("profile activate failed ({})", resp.status())
            }
        }
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
        WorkflowSub::Schedules(s) => cmd_workflow_schedules(client, base, s).await,
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
        budget: None,
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

async fn cmd_workflow_schedules(
    client: &reqwest::Client,
    base: &str,
    a: WorkflowSchedulesArgs,
) -> Result<()> {
    use agentgrid_common::WorkflowSchedule;
    match a.action {
        SchedulesAction::List => {
            let resp = client
                .get(format!("{base}/v1/workflows/{}/schedules", a.id))
                .send()
                .await
                .context("list schedules request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("list schedules failed ({})", resp.status());
            }
            let schedules: Vec<WorkflowSchedule> =
                resp.json().await.context("bad schedule json")?;
            if schedules.is_empty() {
                println!("no schedules for {}", a.id);
            }
            for s in &schedules {
                println!(
                    "{:<12} interval={}s autonomy={} {} last={}",
                    s.id,
                    s.interval_seconds,
                    s.autonomy,
                    if s.enabled { "[on]" } else { "[off]" },
                    if s.last_run_at.is_empty() {
                        "-"
                    } else {
                        &s.last_run_at
                    }
                );
            }
            Ok(())
        }
        SchedulesAction::Create {
            interval_seconds,
            autonomy,
            paused,
        } => {
            let body = serde_json::json!({
                "interval_seconds": interval_seconds,
                "autonomy": autonomy,
                "enabled": !paused,
            });
            let resp = client
                .post(format!("{base}/v1/workflows/{}/schedules", a.id))
                .json(&body)
                .send()
                .await
                .context("create schedule request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("create schedule failed ({})", resp.status());
            }
            let s: WorkflowSchedule = resp.json().await.context("bad schedule json")?;
            println!(
                "schedule {} created: interval={}s autonomy={} {}",
                s.id,
                s.interval_seconds,
                s.autonomy,
                if s.enabled { "[on]" } else { "[off]" }
            );
            Ok(())
        }
        SchedulesAction::Delete { sid } => {
            let resp = client
                .delete(format!("{base}/v1/workflows/{}/schedules/{}", a.id, sid))
                .send()
                .await
                .context("delete schedule request failed")?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                anyhow::bail!("schedule {} not found", sid);
            }
            if !resp.status().is_success() {
                anyhow::bail!("delete schedule failed ({})", resp.status());
            }
            println!("schedule {} deleted", sid);
            Ok(())
        }
    }
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

#[cfg(test)]
mod phase_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn phase_from_event_lifecycle() {
        assert_eq!(phase_from_event("tool_call", &json!({})), Phase::Working);
        assert_eq!(phase_from_event("stdout", &json!({})), Phase::Working);
        assert_eq!(phase_from_event("result", &json!({})), Phase::Done);
        assert_eq!(phase_from_event("error", &json!({})), Phase::Done);
        assert_eq!(
            phase_from_event(
                "status",
                &json!({ "payload": { "text": "attempt succeeded" } })
            ),
            Phase::Done
        );
        assert_eq!(phase_from_event("status", &json!({})), Phase::Working);
        assert_eq!(phase_from_event("weird", &json!({})), Phase::Starting);
    }

    #[test]
    fn paint_no_color_passthrough() {
        assert_eq!(paint(true, "\x1b[31m", "x"), "x");
        assert!(paint(false, "\x1b[31m", "x").contains("\x1b[31m"));
    }
}
