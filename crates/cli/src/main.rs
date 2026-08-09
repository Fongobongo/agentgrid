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

mod phase;
mod tui;
use phase::Phase;
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
    /// One-screen overview: server health, nodes, tasks and workflow runs.
    Status,
    /// Print a shell completion script (bash/zsh/fish/elvish/powershell).
    Completions {
        /// Target shell.
        shell: clap_complete::Shell,
    },
    /// Storage maintenance (artifact GC, disk status).
    Storage(StorageArgs),
    /// Plan 1.3: full-text search over tasks (FTS5).
    Search(SearchArgs),
    /// Plan 1.3: resume a past attempt as a new attempt with inherited context.
    Resume(ResumeArgs),
    /// Plan 1.3: add/remove/list task tags.
    Tag(TagArgs),
    /// Plan 1.4: GitHub issues as tasks (#2b) via the `gh` CLI.
    Issue(IssueArgs),
}

#[derive(Args)]
struct StorageArgs {
    #[command(subcommand)]
    command: StorageSub,
}

/// Plan 1.3: FTS5 search over task prompt/repository.
#[derive(Args)]
struct SearchArgs {
    /// Query text (words to match).
    query: String,
}

/// Plan 1.3: resume an attempt — new attempt inheriting the task's prompt.
#[derive(Args)]
struct ResumeArgs {
    /// Attempt id to resume.
    attempt_id: String,
}

/// Plan 1.3: task tag management.
#[derive(Args)]
struct TagArgs {
    #[command(subcommand)]
    command: TagSub,
}

#[derive(Subcommand)]
enum TagSub {
    /// Add a tag to a task.
    Add { task_id: String, tag: String },
    /// Remove a tag from a task.
    Remove { task_id: String, tag: String },
    /// List a task's tags.
    List { task_id: String },
}

/// Plan 1.4 (#2b): issue-as-task via `gh`.
#[derive(Args)]
struct IssueArgs {
    #[command(subcommand)]
    command: IssueSub,
}

#[derive(Subcommand)]
enum IssueSub {
    /// Create a task from a GitHub issue (`gh issue view <N>` under the hood).
    /// `[repo]` defaults to the current directory's GitHub repo.
    Run { number: i64, repo: Option<String> },
    /// List open issues (`gh issue list`).
    Ls { repo: Option<String> },
    /// Show an issue's title/body (`gh issue view <N>`).
    Show { number: i64, repo: Option<String> },
}

#[derive(Subcommand)]
enum StorageSub {
    /// Reconcile the artifact tree against metadata. `--dry-run` only reports
    /// orphan files / dangling metadata without deleting anything.
    Gc {
        /// Report only; delete nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show free space on the control-plane artifact volume.
    Disk,
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
    /// Hardening P2 item 37: show scheduler eligibility reasoning for the task
    /// even when it is no longer queued (e.g. after assignment), mirroring
    /// `ag task explain`.
    #[arg(long)]
    explain: bool,
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
    /// Password. Omit (or pass `-`) to be prompted via stdin instead of
    /// putting the secret in shell history / `ps`.
    password: Option<String>,
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
        AgCommand::Token(a) => cmd_token(&client, &base, cli.json, a).await,
        AgCommand::Repo(a) => cmd_repo(&client, &base, a).await,
        AgCommand::Login(a) => cmd_login(&client, &base, a).await,
        AgCommand::Approvals(a) => cmd_approvals(&client, &base, a).await,
        AgCommand::Skills(a) => cmd_skills(&client, &base, a).await,
        AgCommand::Mcp(a) => cmd_mcp(&client, &base, a).await,
        AgCommand::Profiles(a) => cmd_profiles(&client, &base, a).await,
        AgCommand::Server(a) => cmd_server_start(a),
        AgCommand::Workflow(a) => cmd_workflow(&client, &base, a, cli.json).await,
        AgCommand::Tui(a) => cmd_tui(&client, &base, a).await,
        AgCommand::Status => cmd_status(&client, &base, cli.json).await,
        AgCommand::Completions { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(shell, &mut Cli::command(), "ag", &mut std::io::stdout());
            Ok(())
        }
        AgCommand::Storage(a) => cmd_storage(&client, &base, a, cli.json).await,
        AgCommand::Search(a) => cmd_search(&client, &base, a, cli.json).await,
        AgCommand::Resume(a) => cmd_resume(&client, &base, a).await,
        AgCommand::Tag(a) => cmd_tag(&client, &base, a, cli.json).await,
        AgCommand::Issue(a) => cmd_issue(&client, &base, a).await,
    }
}

/// Hardening P1 item 15: `ag storage gc [--dry-run]` and `ag storage disk`.
async fn cmd_storage(
    client: &reqwest::Client,
    base: &str,
    a: StorageArgs,
    json: bool,
) -> Result<()> {
    match a.command {
        StorageSub::Gc { dry_run } => {
            let resp = client
                .post(format!("{base}/v1/admin/storage-gc"))
                .json(&serde_json::json!({ "dry_run": dry_run }))
                .send()
                .await
                .context("storage gc request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("storage gc failed: HTTP {}", resp.status());
            }
            let out: serde_json::Value = resp.json().await.context("parse storage gc response")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                let orphans = out
                    .get("orphan_files")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let bytes = out
                    .get("orphan_bytes")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let dangling = out
                    .get("metadata_without_file")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let free_mb = out.get("free_mb").and_then(|v| v.as_u64()).unwrap_or(0);
                if dry_run {
                    println!(
                        "dry-run: {orphans} orphan file(s), {bytes} bytes reclaimable, {dangling} dangling metadata row(s), {free_mb} MB free"
                    );
                } else {
                    println!(
                        "gc: removed {orphans} orphan file(s) ({bytes} bytes), pruned {dangling} dangling metadata row(s); {free_mb} MB free"
                    );
                }
            }
        }
        StorageSub::Disk => {
            // The gc endpoint reports free_mb even with dry_run — reuse it for
            // a cheap disk-status probe without any mutation.
            let resp = client
                .post(format!("{base}/v1/admin/storage-gc"))
                .json(&serde_json::json!({ "dry_run": true }))
                .send()
                .await
                .context("storage disk request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("storage disk failed: HTTP {}", resp.status());
            }
            let out: serde_json::Value =
                resp.json().await.context("parse storage disk response")?;
            let free_mb = out.get("free_mb").and_then(|v| v.as_u64()).unwrap_or(0);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "free_mb": free_mb }))?
                );
            } else {
                println!("control plane artifact volume: {free_mb} MB free");
            }
        }
    }
    Ok(())
}

/// Plan 1.3: FTS5 task search (`ag search <query>`).
async fn cmd_search(client: &reqwest::Client, base: &str, a: SearchArgs, json: bool) -> Result<()> {
    let url = format!("{base}/v1/search?q={}", urlencode(&a.query));
    let resp = client
        .get(&url)
        .send()
        .await
        .context("search request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("search failed ({})", resp.status());
    }
    let hits: Vec<TaskView> = resp.json().await.context("parse search response")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("no tasks match '{}'", a.query);
        return Ok(());
    }
    for t in &hits {
        println!("{:>12}  {:>10}  {}", t.id, t.status, t.prompt);
    }
    Ok(())
}

/// Plan 1.3: resume an attempt — fetch its detail (prompt) and create a fresh
/// task with the same prompt.
async fn cmd_resume(client: &reqwest::Client, base: &str, a: ResumeArgs) -> Result<()> {
    let resp = client
        .get(format!("{base}/v1/attempts/{}", a.attempt_id))
        .send()
        .await
        .context("attempt lookup failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("attempt not found ({})", resp.status());
    }
    let att: agentgrid_common::AttemptView = resp.json().await.context("parse attempt response")?;
    let req = CreateTaskRequest {
        prompt: att.prompt,
        repository: "*".into(),
        adapter: att.adapter,
        requested_node_id: None,
        timeout_secs: None,
        validation_command: None,
        base_commit: None,
        parent_acp_session_id: att.parent_acp_session_id,
        security_profile: None,
        network_mode: None,
    };
    let resp = client
        .post(format!("{base}/v1/tasks"))
        .json(&req)
        .send()
        .await
        .context("resume create failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("resume create failed ({})", resp.status());
    }
    let t: TaskView = resp.json().await.context("parse created task")?;
    println!("resumed attempt {} as task {}", a.attempt_id, t.id);
    Ok(())
}

/// Plan 1.3: tag add/remove/list.
async fn cmd_tag(client: &reqwest::Client, base: &str, a: TagArgs, json: bool) -> Result<()> {
    match a.command {
        TagSub::Add { task_id, tag } => {
            let resp = client
                .post(format!(
                    "{base}/v1/tasks/{task_id}/tags/{}",
                    urlencode(&tag)
                ))
                .send()
                .await
                .context("tag add request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("tag add failed ({})", resp.status());
            }
            println!("tag '{tag}' added to {task_id}");
        }
        TagSub::Remove { task_id, tag } => {
            let resp = client
                .delete(format!(
                    "{base}/v1/tasks/{task_id}/tags/{}",
                    urlencode(&tag)
                ))
                .send()
                .await
                .context("tag remove request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("tag remove failed ({})", resp.status());
            }
            println!("tag '{tag}' removed from {task_id}");
        }
        TagSub::List { task_id } => {
            let resp = client
                .get(format!("{base}/v1/tasks/{task_id}/tags"))
                .send()
                .await
                .context("tag list request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("tag list failed ({})", resp.status());
            }
            let tags: Vec<String> = resp.json().await.context("parse tags")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tags)?);
            } else if tags.is_empty() {
                println!("no tags on {task_id}");
            } else {
                for t in tags {
                    println!("{t}");
                }
            }
        }
    }
    Ok(())
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Plan 1.4 (#2b): run `gh <args>` in `repo` (or the current dir), returning
/// stdout. `gh` is a required runtime dep for the issue commands only.
fn gh_out(repo: Option<&str>, args: &[&str]) -> anyhow::Result<String> {
    let mut cmd = std::process::Command::new("gh");
    if let Some(r) = repo {
        cmd.current_dir(r);
    }
    cmd.args(args);
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("gh not available: {e} (install GitHub CLI)"))?;
    if !out.status.success() {
        anyhow::bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Plan 1.4 (#2b): issue-as-task — `ag issue run/ls/show`.
async fn cmd_issue(client: &reqwest::Client, base: &str, a: IssueArgs) -> Result<()> {
    match a.command {
        IssueSub::Show { number, repo } => {
            let out = gh_out(repo.as_deref(), &["issue", "view", &number.to_string()])?;
            print!("{out}");
            Ok(())
        }
        IssueSub::Ls { repo } => {
            let out = gh_out(repo.as_deref(), &["issue", "list", "--limit", "50"])?;
            print!("{out}");
            Ok(())
        }
        IssueSub::Run { number, repo } => {
            // Fetch title + body via gh (JSON) to build the task prompt.
            let json = gh_out(
                repo.as_deref(),
                &[
                    "issue",
                    "view",
                    &number.to_string(),
                    "--json",
                    "title,body,repository",
                ],
            )?;
            let v: serde_json::Value =
                serde_json::from_str(&json).context("parse gh issue json")?;
            let title = v["title"].as_str().unwrap_or("issue").to_string();
            let body = v["body"].as_str().unwrap_or("").to_string();
            let prompt = if body.trim().is_empty() {
                format!("GitHub issue #{number}: {title}")
            } else {
                format!("GitHub issue #{number}: {title}\n\n{body}")
            };
            let req = CreateTaskRequest {
                prompt,
                repository: "*".into(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: None,
                validation_command: None,
                base_commit: None,
                parent_acp_session_id: None,
                security_profile: None,
                network_mode: None,
            };
            let resp = client
                .post(format!("{base}/v1/tasks"))
                .json(&req)
                .send()
                .await
                .context("issue task create failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("issue task create failed ({})", resp.status());
            }
            let t: TaskView = resp.json().await.context("parse created task")?;
            println!("issue #{number} '{title}' → task {}", t.id);
            Ok(())
        }
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
        security_profile: None,
        network_mode: None,
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
    // Hardening P2 item 37: eligibility reasoning — shown for queued tasks by
    // default (explains why a task is stuck), and for ANY status with
    // `--explain` (why the scheduler picked / rejected nodes).
    if task.status == TaskStatus::Queued || a.explain {
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
    let mut after_ingest: u64 = 0;
    let mut has_ingest = false;
    let mut phase = Phase::Starting;
    loop {
        // Hardening P0 item 9: the global ingest_id cursor orders events
        // across attempts (a retry no longer reorders old vs new attempts).
        // It is only sent once events with a real ingest_id have been seen;
        // before that the legacy per-attempt `after_sequence` cursor is used
        // alone — on old servers/data `ingest_id` is 0, so an `after_ingest`
        // filter would drop every event after the first page.
        let mut query: Vec<(&str, u64)> = Vec::new();
        if has_ingest {
            query.push(("after_ingest", after_ingest));
        } else {
            query.push(("after_sequence", after_ingest));
        }
        let resp = client
            .get(format!("{base}/v1/tasks/{}/events", a.task_id))
            .query(&query)
            .send()
            .await
            .context("events request failed")?;
        if resp.status().is_success() {
            let events: Vec<serde_json::Value> = resp.json().await.context("parse events")?;
            for e in &events {
                let seq = e.get("sequence").and_then(|v| v.as_u64()).unwrap_or(0);
                // Fall back to per-attempt sequence on old servers that never
                // populate ingest_id.
                let ingest = e.get("ingest_id").and_then(|v| v.as_u64()).unwrap_or(0);
                if ingest > 0 {
                    after_ingest = after_ingest.max(ingest);
                    has_ingest = true;
                } else if !has_ingest {
                    after_ingest = after_ingest.max(seq);
                }
                let ty = e.get("type").and_then(|v| v.as_str()).unwrap_or("?");
                phase = Phase::from_event(ty, e);
                print_event(e, seq, ty, nc, e.get("attempt_id").and_then(|v| v.as_str()));
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

fn print_event(e: &serde_json::Value, seq: u64, ty: &str, nc: bool, attempt_id: Option<&str>) {
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
    // Hardening P2 item 37: prefix each event with its attempt id (shortened)
    // so logs from a retried attempt are distinguishable at a glance.
    let seq_tag = match attempt_id {
        Some(aid) => {
            let short: String = aid.chars().take(8).collect();
            format!("[{}] (att-{short})", seq)
        }
        None => format!("[{seq}]"),
    };
    println!("{} {}", paint(nc, C_GRAY, &seq_tag), line);
}

/// Extract `items` from the control plane's ListResponse envelope
/// (`{"items":[...],"next_cursor":...}`); list endpoints no longer return a
/// bare array.
fn list_items(v: &serde_json::Value) -> Vec<serde_json::Value> {
    v.get("items")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Unified API error format (plan 6.1): one consistent message shape for
/// non-2xx responses, with an actionable hint where the cause is obvious
/// (401/403 = missing/expired session -> `ag login`).
fn api_error(status: reqwest::StatusCode, what: &str) -> anyhow::Error {
    use reqwest::StatusCode as S;
    match status {
        S::UNAUTHORIZED | S::FORBIDDEN => {
            anyhow::anyhow!("{what} failed ({status}): not authenticated — run `ag login` first")
        }
        S::NOT_FOUND => anyhow::anyhow!("{what}: not found"),
        s => anyhow::anyhow!("{what} failed ({s})"),
    }
}

/// Plan 6.1: `ag status` — one-screen overview of server, nodes, tasks and
/// workflow runs. Each section is independent: a failing/unauthorized section
/// renders "(unavailable)" instead of aborting the whole overview.
async fn cmd_status(client: &reqwest::Client, base: &str, json: bool) -> Result<()> {
    async fn fetch_items(client: &reqwest::Client, url: String) -> Option<Vec<serde_json::Value>> {
        let resp = client.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        Some(list_items(&v))
    }

    fn count_by_status(items: &[serde_json::Value]) -> std::collections::BTreeMap<String, u64> {
        let mut counts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
        for item in items {
            let st = item
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown")
                .to_string();
            *counts.entry(st).or_default() += 1;
        }
        counts
    }

    let health = client
        .get(format!("{base}/health/ready"))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    let nodes = fetch_items(client, format!("{base}/v1/nodes?limit=200")).await;
    let tasks = fetch_items(client, format!("{base}/v1/tasks?limit=500")).await;
    let runs = fetch_items(client, format!("{base}/v1/workflow-runs?limit=200")).await;

    if json {
        let obj = serde_json::json!({
            "server": base,
            "healthy": health,
            "nodes": nodes.as_ref().map(|n| count_by_status(n)),
            "tasks": tasks.as_ref().map(|t| count_by_status(t)),
            "workflow_runs": runs.as_ref().map(|r| count_by_status(r)),
        });
        println!("{}", serde_json::to_string_pretty(&obj)?);
        return Ok(());
    }

    println!(
        "server    : {base}  ({})",
        if health { "healthy" } else { "UNREACHABLE" }
    );
    let render = |label: &str, items: &Option<Vec<serde_json::Value>>| match items {
        None => println!("{label:<10}: (unavailable — not authenticated? try `ag login`)"),
        Some(list) if list.is_empty() => println!("{label:<10}: none"),
        Some(list) => {
            let counts = count_by_status(list);
            let total: u64 = counts.values().sum();
            let parts: Vec<String> = counts.iter().map(|(k, v)| format!("{v} {k}")).collect();
            println!("{label:<10}: {total} total — {}", parts.join(", "));
        }
    };
    render("nodes", &nodes);
    render("tasks", &tasks);
    render("workflows", &runs);
    Ok(())
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
    let v: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return false,
    };
    list_items(&v)
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
        NodeSub::Doctor { node_id } => cmd_node_doctor(client, base, &node_id).await,
        NodeSub::Drain { node_id, undrain } => {
            cmd_node_drain(client, base, &node_id, undrain).await
        }
    }
}

/// Hardening P2 item 37: drain a node (no NEW assignments; in-flight attempts
/// finish) or undrain it.
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
    if !resp.status().is_success() {
        anyhow::bail!("node lookup failed: HTTP {}", resp.status());
    }
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
            let v: serde_json::Value = resp.json().await.context("bad approvals json")?;
            let approvals: Vec<ApprovalView> =
                serde_json::from_value(serde_json::Value::Array(list_items(&v)))
                    .context("bad approvals json")?;
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

async fn cmd_token(client: &reqwest::Client, base: &str, json: bool, a: TokenArgs) -> Result<()> {
    match a.action {
        TokenAction::Create => {
            let token = create_enrollment_token(client, base).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "token": token }))?
                );
            } else {
                println!("export AGENTGRID_ENROLL_TOKEN={token}");
            }
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
    let password = match &a.password {
        Some(p) if p != "-" => p.clone(),
        _ => {
            // No password (or "-"): read it from stdin so it never appears in
            // shell history or `ps`.
            eprint!("password for {}: ", a.username);
            let mut buf = String::new();
            std::io::stdin()
                .read_line(&mut buf)
                .context("read password from stdin")?;
            let pw = buf.trim_end_matches(['\n', '\r']).to_string();
            if pw.is_empty() {
                anyhow::bail!("no password given (pass it as an argument or on stdin)");
            }
            pw
        }
    };
    let req = LoginRequest {
        username: a.username,
        password,
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
    let v: serde_json::Value = resp.json().await.context("parse workflows response")?;
    let tpls: Vec<WorkflowTemplate> =
        serde_json::from_value(serde_json::Value::Array(list_items(&v)))
            .context("parse workflows response")?;
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
        return Err(api_error(resp.status(), "cancel workflow run"));
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
            let v: serde_json::Value = resp.json().await.context("bad schedule json")?;
            let schedules: Vec<WorkflowSchedule> =
                serde_json::from_value(serde_json::Value::Array(list_items(&v)))
                    .context("bad schedule json")?;
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

#[cfg(test)]
mod phase_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn phase_from_event_lifecycle() {
        assert_eq!(Phase::from_event("tool_call", &json!({})), Phase::Working);
        assert_eq!(Phase::from_event("stdout", &json!({})), Phase::Working);
        assert_eq!(Phase::from_event("result", &json!({})), Phase::Done);
        assert_eq!(Phase::from_event("error", &json!({})), Phase::Done);
        assert_eq!(
            Phase::from_event(
                "status",
                &json!({ "payload": { "text": "attempt succeeded" } })
            ),
            Phase::Done
        );
        assert_eq!(Phase::from_event("status", &json!({})), Phase::Working);
        assert_eq!(Phase::from_event("weird", &json!({})), Phase::Starting);
    }

    #[test]
    fn paint_no_color_passthrough() {
        assert_eq!(paint(true, "\x1b[31m", "x"), "x");
        assert!(paint(false, "\x1b[31m", "x").contains("\x1b[31m"));
    }
}
