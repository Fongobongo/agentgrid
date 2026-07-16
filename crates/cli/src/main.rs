//! Minimal MVP CLI (Stage 1.5): `run`, `logs`, `show`, `nodes`.
//!
//! Command grouping (`task run`, `node list`) is deferred; this flat form
//! exercises the same `/v1` surface.

use agentgrid_common::{CreateTaskRequest, TaskStatus, TaskView};
use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

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
}

#[derive(Args)]
struct RunArgs {
    repository: String,
    prompt: String,
    #[arg(long, default_value = "mock")]
    adapter: String,
    #[arg(long)]
    node: Option<String>,
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();
    let base = cli.server.trim_end_matches('/').to_string();

    match cli.command {
        AgCommand::Run(a) => cmd_run(&client, &base, a).await,
        AgCommand::Logs(a) => cmd_logs(&client, &base, a).await,
        AgCommand::Show(a) => cmd_show(&client, &base, a).await,
        AgCommand::Nodes => cmd_node_list(&client, &base).await,
        AgCommand::Cancel(a) => cmd_cancel(&client, &base, a).await,
        AgCommand::Retry(a) => cmd_retry(&client, &base, a).await,
    }
}

async fn cmd_run(client: &reqwest::Client, base: &str, a: RunArgs) -> Result<()> {
    let req = CreateTaskRequest {
        prompt: a.prompt,
        repository: a.repository,
        adapter: a.adapter,
        requested_node_id: a.node,
        timeout_secs: None,
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

async fn cmd_show(client: &reqwest::Client, base: &str, a: ShowArgs) -> Result<()> {
    let resp = client
        .get(format!("{base}/v1/tasks/{}", a.task_id))
        .send()
        .await
        .context("show request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("task not found ({})", resp.status());
    }
    let task: TaskView = resp.json().await.context("parse task response")?;
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

async fn cmd_node_list(client: &reqwest::Client, base: &str) -> Result<()> {
    let resp = client
        .get(format!("{base}/v1/nodes"))
        .send()
        .await
        .context("node list request failed")?;
    let nodes: Vec<serde_json::Value> = resp.json().await.context("parse nodes")?;
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
