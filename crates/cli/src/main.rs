//! Minimal MVP CLI (Stage 1.5): `run`, `logs`, `show`, `nodes`.
//!
//! Command grouping (`task run`, `node list`) is deferred; this flat form
//! exercises the same `/v1` surface.

use agentgrid_common::{
    CreateTaskRequest, LoginRequest, LoginResponse, TaskEligibility, TaskStatus, TaskView,
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
    /// List registered nodes.
    Nodes,
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
    /// Start the control plane (standalone binary).
    Server(ServerStartArgs),
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
        AgCommand::Nodes => cmd_node_list(&client, &base, cli.json).await,
        AgCommand::Cancel(a) => cmd_cancel(&client, &base, a).await,
        AgCommand::Retry(a) => cmd_retry(&client, &base, a).await,
        AgCommand::Token(a) => cmd_token(&client, &base, a).await,
        AgCommand::Repo(a) => cmd_repo(&client, &base, a).await,
        AgCommand::Login(a) => cmd_login(&client, &base, a).await,
        AgCommand::Server(a) => cmd_server_start(a),
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
    println!("{:<36} {:<10} {:<8} {:<6}", "ID", "STATUS", "ACTIVE", "MAX");
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
        println!("{id:<36} {st:<10} {active:<8} {max:<6}");
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
            let resp = client
                .post(format!("{base}/v1/nodes/enrollment-token"))
                .send()
                .await
                .context("enrollment-token request failed")?;
            if !resp.status().is_success() {
                anyhow::bail!("token creation failed ({})", resp.status());
            }
            let body: serde_json::Value = resp.json().await?;
            let token = body
                .get("token")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            println!("export AGENTGRID_ENROLL_TOKEN={token}");
            Ok(())
        }
    }
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
    if let Some(p) = &a.bootstrap_password {
        cmd.env("AGENTGRID_BOOTSTRAP_PASSWORD", p);
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
