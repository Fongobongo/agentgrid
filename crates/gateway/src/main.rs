//! Chat gateway: bridge a chat platform (Telegram first) to the control-plane
//! HTTP API so an operator can drive the grid from a phone.
//!
//! One provider trait, one Telegram implementation (raw reqwest to the Telegram
//! Bot API; no chat-client crate). Discord / WhatsApp are stubbed behind the
//! same trait — see the "not implemented" arms. WhatsApp in particular has no
//! easy open bot API (Business API is heavy and gated), so it is honestly
//! deferred rather than faked.
//!
//! Auth: a comma-separated allowlist of chat ids in `AGENTGRID_GATEWAY_ADMINS`.
//! Any chat not on the list is ignored.
//!
//! Commands: /help /nodes /tasks /run <repo> <adapter> <prompt...>
//!           /show <id> /cancel <id> /logs <id>

use std::time::Duration;

use agentgrid_common::CreateTaskRequest;
use anyhow::Result;

#[derive(clap::Parser)]
struct Args {
    /// Control-plane base URL, e.g. http://127.0.0.1:7800
    #[arg(long, env = "AGENTGRID_SERVER")]
    control_plane: String,
    /// A JWT for a control-plane user (operator). Created with `ag login`.
    #[arg(long, env = "AGENTGRID_GATEWAY_TOKEN")]
    token: String,
    /// Telegram bot token from @BotFather. Omit to disable Telegram.
    #[arg(long, env = "AGENTGRID_GATEWAY_TELEGRAM_TOKEN")]
    telegram: Option<String>,
    /// Comma-separated allowlist of numeric chat ids allowed to talk to the
    /// gateway. Any other chat is ignored.
    #[arg(long, env = "AGENTGRID_GATEWAY_ADMINS")]
    admins: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agentgrid_gateway=info".into()),
        )
        .init();
    let args: Args = clap::Parser::parse();
    let admins: Vec<i64> = args
        .admins
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    if admins.is_empty() {
        anyhow::bail!("no admin chat ids in --admins");
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let ctl = ControlPlane::new(&client, &args.control_plane, &args.token);

    let provider: Box<dyn ChatProvider> = if let Some(tok) = args.telegram.as_deref() {
        Box::new(Telegram::new(tok.to_string()))
    } else {
        tracing::warn!("no chat provider configured (only --telegram supported); nothing to do");
        return Ok(());
    };
    tracing::info!(
        "gateway up: provider=telegram, control_plane={}",
        args.control_plane
    );
    provider.run(&client, &ctl, &admins).await
}

/// A control-plane HTTP client for the handful of endpoints the gateway uses.
struct ControlPlane<'a> {
    client: &'a reqwest::Client,
    base: &'a str,
    token: &'a str,
}

impl<'a> ControlPlane<'a> {
    fn new(client: &'a reqwest::Client, base: &'a str, token: &'a str) -> Self {
        Self {
            client,
            base,
            token,
        }
    }
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(format!("{}{}", self.base, path))
            .bearer_auth(self.token)
    }
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{}", self.base, path))
            .bearer_auth(self.token)
    }

    async fn nodes(&self) -> Result<String> {
        let v: serde_json::Value = self.get("/v1/nodes").send().await?.json().await?;
        Ok(fmt_nodes(&v))
    }
    async fn tasks(&self) -> Result<String> {
        let v: serde_json::Value = self.get("/v1/tasks").send().await?.json().await?;
        Ok(fmt_tasks(&v))
    }
    async fn show(&self, id: &str) -> Result<String> {
        let r = self.get(&format!("/v1/tasks/{id}")).send().await?;
        if !r.status().is_success() {
            return Ok(format!("task {id} not found ({})", r.status()));
        }
        let v: serde_json::Value = r.json().await?;
        let st = v.get("status").and_then(|x| x.as_str()).unwrap_or("?");
        let p = v.get("prompt").and_then(|x| x.as_str()).unwrap_or("");
        let repo = v.get("repository").and_then(|x| x.as_str()).unwrap_or("?");
        let adapter = v.get("adapter").and_then(|x| x.as_str()).unwrap_or("?");
        Ok(format!(
            "task {id}\nstatus: {st}\nrepo: {repo}\nadapter: {adapter}\nprompt: {p}"
        ))
    }
    async fn run(&self, repo: &str, adapter: &str, prompt: &str) -> Result<String> {
        let req = CreateTaskRequest {
            prompt: prompt.to_string(),
            repository: repo.to_string(),
            adapter: adapter.to_string(),
            requested_node_id: None,
            timeout_secs: None,
            validation_command: None,
            base_commit: None,
        };
        let r = self.post("/v1/tasks").json(&req).send().await?;
        let status = r.status();
        let body = r.text().await.unwrap_or_default();
        if !status.is_success() {
            return Ok(format!("create task failed ({status}): {body}"));
        }
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("?");
        let st = v.get("status").and_then(|x| x.as_str()).unwrap_or("?");
        Ok(format!("task {id} created ({st})"))
    }
    async fn cancel(&self, id: &str) -> Result<String> {
        let r = self.post(&format!("/v1/tasks/{id}/cancel")).send().await?;
        Ok(format!("cancel {id}: {}", r.status()))
    }
    async fn logs(&self, id: &str) -> Result<String> {
        let r = self.get(&format!("/v1/tasks/{id}/events")).send().await?;
        let v: serde_json::Value = r.json().await.unwrap_or(serde_json::Value::Array(vec![]));
        let arr = v.as_array().cloned().unwrap_or_default();
        if arr.is_empty() {
            return Ok(format!("no events for {id}"));
        }
        let mut out = String::new();
        for (i, e) in arr.iter().take(20).enumerate() {
            let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
            let data = e.get("data").map(|v| v.to_string()).unwrap_or_default();
            out.push_str(&format!("{} {kind}: {data}\n", i));
        }
        if arr.len() > 20 {
            out.push_str(&format!("... ({} more)\n", arr.len() - 20));
        }
        Ok(out)
    }
}

/// A chat platform the gateway can speak to: receive messages and reply.
trait ChatProvider: Send {
    /// Run the receive/dispatch loop until the process is asked to stop.
    fn run<'a>(
        self: Box<Self>,
        client: &'a reqwest::Client,
        ctl: &'a ControlPlane<'a>,
        admins: &'a [i64],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;
}

fn allowed(chat_id: i64, admins: &[i64]) -> bool {
    admins.contains(&chat_id)
}

async fn dispatch(ctl: &ControlPlane<'_>, text: &str) -> String {
    let mut parts = text.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    // strip an optional leading bot mention like "/nodes@botname"
    let cmd = cmd.split('@').next().unwrap_or(cmd).trim_start_matches('/');
    match cmd {
        "help" | "start" => HELP.to_string(),
        "nodes" => ctl.nodes().await.unwrap_or_else(|e| e.to_string()),
        "tasks" => ctl.tasks().await.unwrap_or_else(|e| e.to_string()),
        "show" => match parts.next() {
            Some(id) => ctl.show(id).await.unwrap_or_else(|e| e.to_string()),
            None => "usage: /show <task-id>".into(),
        },
        "cancel" => match parts.next() {
            Some(id) => ctl.cancel(id).await.unwrap_or_else(|e| e.to_string()),
            None => "usage: /cancel <task-id>".into(),
        },
        "logs" => match parts.next() {
            Some(id) => ctl.logs(id).await.unwrap_or_else(|e| e.to_string()),
            None => "usage: /logs <task-id>".into(),
        },
        "run" => {
            let repo = parts.next();
            let adapter = parts.next();
            let prompt: String = parts.collect::<Vec<_>>().join(" ");
            match (repo, adapter) {
                (Some(repo), Some(adapter)) if !prompt.is_empty() => ctl
                    .run(repo, adapter, &prompt)
                    .await
                    .unwrap_or_else(|e| e.to_string()),
                _ => "usage: /run <repo-url> <adapter> <prompt...>".into(),
            }
        }
        _ => format!("unknown command: {cmd} — try /help"),
    }
}

const HELP: &str = "agentgrid gateway — /help /nodes /tasks /show <id> /cancel <id> /logs <id> /run <repo-url> <adapter> <prompt...>";

// ---- formatting ----

fn fmt_nodes(v: &serde_json::Value) -> String {
    let arr = match v.as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return "(no nodes)".into(),
    };
    let mut s = format!(
        "{:<12} {:<10} {:<3}/{:<3} {:<10}\n",
        "NODE", "STATUS", "ACT", "MAX", "DISK"
    );
    for n in arr {
        let name = n.get("name").and_then(|v| v.as_str()).unwrap_or("-");
        let st = n.get("status").and_then(|v| v.as_str()).unwrap_or("-");
        let act = n
            .get("active_attempts")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let max = n
            .get("max_concurrency")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let disk = n.get("free_disk_mb").and_then(|v| v.as_u64()).unwrap_or(0);
        let disk = if disk < 1024 {
            format!("{disk} MB !")
        } else {
            format!("{:.0} GB", disk as f64 / 1024.0)
        };
        s.push_str(&format!(
            "{name:<12} {st:<10} {act:<3}/{max:<3} {disk:<10}\n"
        ));
    }
    s
}

fn fmt_tasks(v: &serde_json::Value) -> String {
    let arr = match v.as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return "(no tasks)".into(),
    };
    let mut s = format!("{:<12} {:<36} {:<12}\n", "REPO", "ID", "STATUS");
    for t in arr {
        let id = t.get("id").and_then(|v| v.as_str()).unwrap_or("-");
        let st = t.get("status").and_then(|v| v.as_str()).unwrap_or("-");
        let repo = t.get("repository").and_then(|v| v.as_str()).unwrap_or("-");
        s.push_str(&format!("{repo:<12} {id:<36} {st:<12}\n"));
    }
    s
}

// ---- Telegram provider (raw Bot API over reqwest, no chat crate) ----

struct Telegram {
    token: String,
    offset: std::sync::atomic::AtomicI64,
}

impl Telegram {
    fn new(token: String) -> Self {
        Self {
            token,
            offset: std::sync::atomic::AtomicI64::new(0),
        }
    }
    fn url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }
}

impl ChatProvider for Telegram {
    fn run<'a>(
        self: Box<Self>,
        client: &'a reqwest::Client,
        ctl: &'a ControlPlane<'a>,
        admins: &'a [i64],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        let tg = self;
        Box::pin(async move {
            loop {
                let offset = tg.offset.load(std::sync::atomic::Ordering::Relaxed);
                let resp: serde_json::Value = match client
                    .post(tg.url("getUpdates"))
                    .json(&serde_json::json!({"offset": offset, "timeout": 30}))
                    .send()
                    .await
                {
                    Ok(r) => match r.json().await {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("getUpdates parse: {e}");
                            tokio::time::sleep(Duration::from_secs(3)).await;
                            continue;
                        }
                    },
                    Err(e) => {
                        tracing::warn!("getUpdates: {e}");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                };
                let updates = resp
                    .get("result")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                for u in updates {
                    let id = u.get("update_id").and_then(|v| v.as_i64()).unwrap_or(0);
                    tg.offset
                        .store(id + 1, std::sync::atomic::Ordering::Relaxed);
                    let msg = match u.get("message").or_else(|| u.get("edited_message")) {
                        Some(m) => m,
                        None => continue,
                    };
                    let chat_id = msg
                        .get("chat")
                        .and_then(|c| c.get("id"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let text = msg
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !text.starts_with('/') {
                        continue;
                    }
                    if !allowed(chat_id, admins) {
                        tracing::info!("ignoring chat {chat_id} (not in allowlist)");
                        continue;
                    }
                    tracing::info!("tg {chat_id}: {text}");
                    let reply = dispatch(ctl, &text).await;
                    let _ = client
                        .post(tg.url("sendMessage"))
                        .json(&serde_json::json!({"chat_id": chat_id, "text": reply}))
                        .send()
                        .await;
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_nodes_marks_low_disk() {
        let v: serde_json::Value = serde_json::json!([
            {"name":"a","status":"online","active_attempts":0,"max_concurrency":2,"free_disk_mb":500},
            {"name":"b","status":"degraded","active_attempts":1,"max_concurrency":4,"free_disk_mb":4096}
        ]);
        let s = fmt_nodes(&v);
        assert!(s.contains("500 MB !"));
        assert!(s.contains("4 GB"));
        assert!(s.contains("degraded"));
    }

    #[test]
    fn fmt_nodes_empty() {
        assert_eq!(fmt_nodes(&serde_json::Value::Array(vec![])), "(no nodes)");
    }

    #[test]
    fn fmt_tasks_lists_rows() {
        let v: serde_json::Value = serde_json::json!([
            {"id":"abc","status":"running","repository":"r1"}
        ]);
        let s = fmt_tasks(&v);
        assert!(s.contains("abc"));
        assert!(s.contains("running"));
        assert!(s.contains("r1"));
    }
}
