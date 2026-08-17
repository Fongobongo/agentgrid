//! MCP server discovery: fetch enabled servers from the control plane and
//! project them into stdio spawn descriptions for the adapter.

use reqwest::Client;
use serde_json::{json, Value};

/// Fetch enabled MCP servers (optionally filtered to `subset`) and project
/// each into a stdio spawn description (name + command + args +
/// env_requirements). Returns `Value::Null` on any error so a session never
/// blocks on MCP wiring — the adapter simply runs without MCP.
pub async fn mcp_servers_payload(client: &Client, server: &str, subset: &[String]) -> Value {
    let resp = match client.get(format!("{server}/v1/mcp-servers")).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("mcp-servers fetch failed: {e}; session runs without MCP");
            return Value::Null;
        }
    };
    if !resp.status().is_success() {
        return Value::Null;
    }
    let servers: Vec<agentgrid_common::McpServer> = match resp.json().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("mcp-servers decode failed: {e}");
            return Value::Null;
        }
    };
    let enabled: Vec<_> = servers
        .into_iter()
        .filter(|s| s.enabled)
        .filter(|s| subset.is_empty() || subset.iter().any(|id| id == &s.id))
        .collect();
    if enabled.is_empty() {
        return Value::Null;
    }
    // Project each server into a stdio spawn description (name + command +
    // args). Env env_requirements are env var *names* the node resolves at
    // spawn — sent as names so the adapter can check they're set in its env
    // (the node forwards cfg.adapter_env into the spawn env separately).
    let servers_json: Vec<Value> = enabled
        .iter()
        .map(|s| {
            json!({
                "name": s.id,
                "label": s.name,
                "command": s.command,
                "args": s.args,
                "env_requirements": s.env_requirements,
            })
        })
        .collect();
    json!({ "servers": servers_json })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Accept anything on a port and answer 200 OK with `body`. Serves a full
    /// HTTP/1.1 keep-alive connection: reqwest pools the connection, and a
    /// handler that exits after one response races a client reuse — the pooled
    /// second request hits a closed socket and dies on a TCP RST (flaky `Null`
    /// payloads under 16-thread test stress). Looping until the client closes
    /// keeps pooled connections serviceable.
    async fn dummy_profile_server(body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(), body
                    );
                    loop {
                        match s.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        if s.write_all(resp.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn mcp_payload_projects_enabled_servers_and_drops_disabled() {
        // Stage 13: the MCP block includes enabled servers from the
        // registry and excludes disabled ones; an unreachable CP yields Null.
        let body = r#"[
            {"id":"github","name":"GitHub","command":"mcp-github","args":["--ro"],"env_requirements":["GITHUB_TOKEN"],"enabled":true,"created_at":""},
            {"id":"legacy","name":"Legacy","command":"mcp-old","args":[],"env_requirements":[],"enabled":false,"created_at":""}
        ]"#;
        let server = dummy_profile_server(body).await;
        let client = reqwest::Client::new();
        let val = mcp_servers_payload(&client, &server, &[]).await;
        let servers = val.get("servers").expect("enabled servers present");
        let arr = servers.as_array().unwrap();
        assert_eq!(
            arr.len(),
            1,
            "disabled server is dropped (fail-closed gate)"
        );
        assert_eq!(arr[0]["name"], "github");
        assert_eq!(arr[0]["command"], "mcp-github");
        // No secret value bytes leak through.
        let text = val.to_string();
        assert!(!text.contains("ghp_"), "no secret bytes: {text}");
    }

    #[tokio::test]
    async fn mcp_payload_subset_filters_to_profile_allow_list() {
        // Stage 13 follow-up: a non-empty per-profile subset attaches only the
        // listed registry servers (by id), even if more are enabled. Empty
        // subset = all enabled (covered above).
        let body = r#"[
            {"id":"github","name":"GitHub","command":"mcp-github","args":[],"env_requirements":[],"enabled":true,"created_at":""},
            {"id":"fs","name":"FS","command":"mcp-fs","args":[],"env_requirements":[],"enabled":true,"created_at":""},
            {"id":"legacy","name":"Legacy","command":"mcp-old","args":[],"env_requirements":[],"enabled":false,"created_at":""}
        ]"#;
        let server = dummy_profile_server(body).await;
        let client = reqwest::Client::new();
        let val = mcp_servers_payload(&client, &server, &["github".into()]).await;
        let arr = val.get("servers").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 1, "subset keeps only the listed id");
        assert_eq!(arr[0]["name"], "github");
        // Empty subset = all enabled (2 here; legacy disabled dropped).
        let val = mcp_servers_payload(&client, &server, &[]).await;
        assert_eq!(val.get("servers").unwrap().as_array().unwrap().len(), 2);
    }
}
