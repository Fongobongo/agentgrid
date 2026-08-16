//! Node-side WebSocket control channel client (plan 0.3 2.3 / ADR 0009).
//!
//! The WS channel carries only control messages (assignment push, ack, cancel,
//! heartbeat); the data plane (events, completions, artifacts) stays HTTP.
//! `ws_loop` reconnects forever with exponential backoff; `auto_loop` falls
//! back to long polling when the WS connection repeatedly fails.

use std::sync::Arc;
use std::time::Duration;

use agentgrid_common::ws::NodeWsMsg;
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use tokio::sync::Semaphore;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;

use crate::config::{Config, SavedCredential};
use crate::polling::{dispatch_batch, poll_loop_inner};

/// Consecutive WS connection failures tolerated before `auto` transport runs
/// the poll fallback window.
const MAX_CONSECUTIVE_WS_FAILURES: usize = 3;
/// Slot heartbeat interval on the WS channel (the HTTP heartbeat still runs).
const WS_HEARTBEAT_INTERVAL_SECS: u64 = 30;

type WsStream = tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

fn ws_url(server: &str) -> String {
    if let Some(rest) = server.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = server.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        server.to_string()
    }
}

/// Connect and send the `Hello` handshake; returns the open socket.
async fn connect_once(cfg: &Config, cred: &SavedCredential) -> Result<WsStream> {
    let url = format!("{}/v1/node/ws", ws_url(&cfg.server));
    let mut req = url.into_client_request()?;
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {}", cred.credential).parse()?,
    );
    let (mut sock, _resp) = tokio_tungstenite::connect_async(req).await?;
    let hello = NodeWsMsg::Hello {
        node_id: cred.node_id.clone(),
        name: cfg.node_name.clone(),
        adapters: cfg.adapters.iter().map(|s| s.id.clone()).collect(),
        repositories: cfg.repositories.clone(),
        max_concurrency: cfg.max_concurrency,
        protocol_version: Some(agentgrid_common::NODE_PROTOCOL_VERSION.into()),
        agent_version: cfg.agent_version.clone(),
    };
    sock.send(Message::Text(serde_json::to_string(&hello)?.into()))
        .await?;
    Ok(sock)
}

/// Run one connected session until the socket closes or errors.
async fn run_session(
    mut sock: WsStream,
    cfg: &Config,
    client: &Client,
    sem: &Arc<Semaphore>,
    cred: &SavedCredential,
) -> Result<()> {
    tracing::info!("ws control channel connected to {}", cfg.server);
    let mut hb = tokio::time::interval(Duration::from_secs(WS_HEARTBEAT_INTERVAL_SECS));
    hb.tick().await; // consume the immediate first tick
    loop {
        tokio::select! {
            _ = hb.tick() => {
                let msg = NodeWsMsg::Heartbeat { free_slots: sem.available_permits() as u32 };
                sock.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
            }
            msg = sock.next() => {
                let Some(msg) = msg else { return Ok(()) };
                match msg? {
                    Message::Text(t) => handle_msg(&t, cfg, client, sem, &mut sock, cred).await?,
                    Message::Ping(p) => sock.send(Message::Pong(p)).await?,
                    Message::Close(cf) => {
                        tracing::info!("ws closed by control plane: {cf:?}");
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn handle_msg(
    text: &str,
    cfg: &Config,
    client: &Client,
    sem: &Arc<Semaphore>,
    sock: &mut WsStream,
    cred: &SavedCredential,
) -> Result<()> {
    let send = |msg: &NodeWsMsg| -> Result<Message> {
        Ok(Message::Text(serde_json::to_string(msg)?.into()))
    };
    match serde_json::from_str::<NodeWsMsg>(text) {
        Ok(NodeWsMsg::Assignment { assignments }) if !assignments.is_empty() => {
            let ids: Vec<String> = assignments.iter().map(|a| a.attempt_id.clone()).collect();
            // Echo the fencing tokens back with the ack (plan 0.3 2.4).
            let tokens: Vec<String> = assignments
                .iter()
                .map(|a| a.fencing_token.clone())
                .collect();
            dispatch_batch(cfg, client, sem, assignments).await?;
            // Receipt ack; the authoritative "agent started" ack still comes
            // from the attempt runner over HTTP.
            let ack = NodeWsMsg::Ack {
                attempt_ids: ids,
                fencing_tokens: tokens,
                ok: true,
                error: None,
            };
            sock.send(send(&ack)?).await?;
        }
        Ok(NodeWsMsg::Cancel { attempt_id }) => {
            tracing::info!("ws cancel for {attempt_id}");
            crate::completion::notify_cancel(&attempt_id).await;
            sock.send(send(&NodeWsMsg::CancelAck { attempt_id })?)
                .await?;
        }
        Ok(NodeWsMsg::HelloOk { .. }) => {}
        // Feature "opencode profiles": the CP nudges us when the assigned
        // profile changed. We do the authoritative pull ourselves — the push
        // only carries the hash so a node never trusts a man-in-the-middle
        // (best-effort; the pull is a bit of JSON over the same channel).
        Ok(NodeWsMsg::ConfigUpdate { hash, .. }) => {
            if let Err(e) = crate::opencode_config::pull_and_apply(
                cfg,
                client,
                cred,
                "ws_push",
                hash.as_deref(),
            )
            .await
            {
                tracing::warn!("opencode config apply on ws_push failed: {e}");
            }
        }
        Ok(other) => tracing::debug!("unexpected ws message: {other:?}"),
        Err(e) => tracing::warn!("bad ws message: {e}"),
    }
    Ok(())
}

/// WS transport: reconnect with exponential backoff (1s → 60s) forever, so a
/// CP restart or network drop heals itself without manual intervention.
pub async fn ws_loop(
    cfg: Config,
    cred: SavedCredential,
    client: Client,
    sem: Arc<Semaphore>,
) -> Result<()> {
    let mut backoff = Duration::from_secs(1);
    loop {
        match connect_once(&cfg, &cred).await {
            Ok(sock) => {
                backoff = Duration::from_secs(1);
                if let Err(e) = run_session(sock, &cfg, &client, &sem, &cred).await {
                    tracing::warn!("ws session ended: {e}");
                }
            }
            Err(e) => tracing::warn!("ws connect failed: {e}"),
        }
        tracing::info!("ws reconnect in {backoff:?}");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// Auto transport: prefer WS; after `MAX_CONSECUTIVE_WS_FAILURES` failed
/// connects, run long polling for the current backoff window, then retry WS.
/// A mid-session drop (CP restart, network loss) reconnects promptly.
pub async fn auto_loop(
    cfg: Config,
    cred: SavedCredential,
    client: Client,
    sem: Arc<Semaphore>,
) -> Result<()> {
    let mut backoff = Duration::from_secs(1);
    let mut consecutive_failures = 0usize;
    loop {
        match connect_once(&cfg, &cred).await {
            Ok(sock) => {
                consecutive_failures = 0;
                backoff = Duration::from_secs(1);
                if let Err(e) = run_session(sock, &cfg, &client, &sem, &cred).await {
                    tracing::warn!("ws session ended: {e}");
                }
                // Reconnect promptly after a mid-session drop.
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                consecutive_failures += 1;
                tracing::warn!("ws connect failed ({consecutive_failures}): {e}");
                if consecutive_failures >= MAX_CONSECUTIVE_WS_FAILURES {
                    tracing::info!("falling back to poll transport for {backoff:?}");
                    poll_loop_inner(
                        cfg.clone(),
                        client.clone(),
                        sem.clone(),
                        cred.node_id.clone(),
                        Some(backoff),
                    )
                    .await?;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    consecutive_failures = 0;
                } else {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! 2.3 acceptance: the node survives a CP restart on the same address and
    //! returns to the registry without manual intervention; assignments are
    //! pushed over the restored channel.

    use super::*;
    use crate::config::Transport;
    use agentgrid_control_plane::{build_router, AppState};
    use serde_json::json;
    use std::time::Instant;

    async fn login(base: &str) -> String {
        reqwest::Client::new()
            .post(format!("{base}/v1/auth/login"))
            .json(&json!({"username": "test", "password": "test"}))
            .send()
            .await
            .unwrap()
            .json::<agentgrid_common::LoginResponse>()
            .await
            .unwrap()
            .token
    }

    async fn enroll(base: &str, token: &str) -> (String, String) {
        let http = reqwest::Client::new();
        let tok: String = http
            .post(format!("{base}/v1/nodes/enrollment-token"))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap()
            .json::<agentgrid_common::EnrollTokenResponse>()
            .await
            .unwrap()
            .token;
        let er = http
            .post(format!("{base}/v1/node/enroll"))
            .json(&json!({
                "token": tok,
                "name": "ws-reconnect-node",
                "adapters": ["mock"],
                "repositories": ["*"],
                "max_concurrency": 1,
            }))
            .send()
            .await
            .unwrap()
            .json::<agentgrid_common::EnrollResponse>()
            .await
            .unwrap();
        (er.node_id, er.credential)
    }

    async fn create_task(base: &str, token: &str) -> String {
        reqwest::Client::new()
            .post(format!("{base}/v1/tasks"))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({
                "prompt": "ws reconnect task",
                "repository": "*",
                "adapter": "mock",
                "timeout_secs": 600,
            }))
            .send()
            .await
            .unwrap()
            .json::<agentgrid_common::TaskView>()
            .await
            .unwrap()
            .id
    }

    fn test_cfg(server: &str, dir: &std::path::Path) -> Config {
        std::fs::create_dir_all(dir).unwrap();
        Config {
            server: server.to_string(),
            node_name: "ws-reconnect-node".into(),
            workspace_root: dir.join("ws"),
            max_concurrency: 1,
            agent_version: "test".into(),
            adapters: vec![crate::config::AdapterSpec {
                id: "mock".into(),
                protocol: crate::config::AdapterProtocol::Wrapper,
            }],
            repositories: vec!["*".into()],
            heartbeat_secs: 5,
            enroll_token: None,
            credential_path: dir.join("credential.json"),
            env_file: None,
            repository_root: dir.join("repos"),
            secrets: vec![],
            adapter_env: vec![],
            sandbox: crate::sandbox::SandboxKind::None,
            outbox_root: dir.join("outbox"),
            artifact_spool_root: dir.join("spool"),
            max_artifact_size: 1024 * 1024,
            completion_outbox: Arc::new(
                crate::outbox::CompletionOutbox::open(&dir.join("outbox")).unwrap(),
            ),
            autonomy: Default::default(),
            adapter_versions: Default::default(),
            network_mode: "none".into(),
            transport: Transport::Ws,
            guard_deny: vec![],
            guard_allow: vec![],
            accounts: vec![],
        }
    }

    /// Node HTTP client carrying the node credential, exactly like
    /// `run_transport` builds it — the attempt runner's data-plane calls
    /// (events/completion/artifacts) authenticate with this header.
    fn authed_client(cred: &SavedCredential) -> reqwest::Client {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", cred.credential).parse().unwrap(),
        );
        reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap()
    }

    async fn wait_until<F, Fut>(desc: &str, timeout: Duration, mut f: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if f().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for: {desc}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ws_node_survives_cp_restart() {
        let state = AppState::open_temp().await.unwrap();
        // Bind with reuseaddr so the "restart" can take the same port.
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        sock.set_reuseaddr(true).unwrap();
        sock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = sock.local_addr().unwrap();
        let listener = sock.listen(64).unwrap();
        let st1 = state.clone();
        let server1 = tokio::spawn(async move {
            let _ = axum::serve(listener, build_router(st1)).await;
        });
        let base = format!("http://{addr}");
        let token = login(&base).await;
        let (node_id, credential) = enroll(&base, &token).await;

        let dir = std::env::temp_dir().join(format!(
            "ag-ws-node-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let cfg = test_cfg(&base, &dir);
        let cred = SavedCredential {
            node_id: node_id.clone(),
            credential,
        };
        let sem = Arc::new(Semaphore::new(cfg.max_concurrency as usize));
        let http = authed_client(&cred);
        let node_task = tokio::spawn(ws_loop(cfg, cred, http, sem));

        // 1) node registers automatically.
        let st = state.clone();
        wait_until("ws registration", Duration::from_secs(10), || async {
            st.ws_registry.connection_count().await == 1
        })
        .await;

        // 2) CP restart: kill the server, bring it back on the same address,
        // and the node must re-register on its own. (Do not assert the
        // registry is empty in between — the node reconnects within ~1 s.)
        server1.abort();
        let _ = server1.await;
        let sock2 = tokio::net::TcpSocket::new_v4().unwrap();
        sock2.set_reuseaddr(true).unwrap();
        sock2.bind(addr).unwrap();
        let listener2 = sock2.listen(64).unwrap();
        let st2 = state.clone();
        let _server2 = tokio::spawn(async move {
            let _ = axum::serve(listener2, build_router(st2)).await;
        });
        let st = state.clone();
        wait_until(
            "re-registration after CP restart",
            Duration::from_secs(15),
            || async { st.ws_registry.connection_count().await == 1 },
        )
        .await;

        // 3) assignments flow over the restored channel: the receipt Ack flips
        // the task to running.
        let task_id = create_task(&base, &token).await;
        wait_until(
            "task running via ws push",
            Duration::from_secs(15),
            || async {
                let Ok(r) = reqwest::Client::new()
                    .get(format!("{base}/v1/tasks/{task_id}"))
                    .header("authorization", format!("Bearer {token}"))
                    .send()
                    .await
                else {
                    return false;
                };
                r.text().await.unwrap_or_default().contains("\"running\"")
            },
        )
        .await;

        node_task.abort();
    }

    /// Plan 0.3 2.4 failure injection: kill the CP mid-attempt. The attempt
    /// must complete once the CP is back, and its events must all land
    /// (durable event outbox + idempotent ingest + bounded retry).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ws_attempt_survives_cp_kill_midflight() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "ag-fi-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        // A self-contained `adapter-mock` (JSON-line protocol): 5 log events,
        // a 4 s sleep spanning the CP outage, 5 more events, clean result.
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let script = bin_dir.join("adapter-mock");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             for i in 1 2 3 4 5; do echo '{\"type\":\"log\",\"payload\":{\"text\":\"pre\"}}'; done\n\
             sleep 4\n\
             for i in 1 2 3 4 5; do echo '{\"type\":\"log\",\"payload\":{\"text\":\"post\"}}'; done\n\
             echo '{\"type\":\"result\",\"payload\":{\"exit_code\":0,\"text\":\"done\"}}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", bin_dir.display()));

        let state = AppState::open_temp().await.unwrap();
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        sock.set_reuseaddr(true).unwrap();
        sock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = sock.local_addr().unwrap();
        let listener = sock.listen(64).unwrap();
        let st1 = state.clone();
        let server1 = tokio::spawn(async move {
            let _ = axum::serve(listener, build_router(st1)).await;
        });
        let base = format!("http://{addr}");
        let token = login(&base).await;
        let (node_id, credential) = enroll(&base, &token).await;

        let cfg = test_cfg(&base, &dir.join("data"));
        let cred = SavedCredential {
            node_id: node_id.clone(),
            credential,
        };
        let sem = Arc::new(Semaphore::new(cfg.max_concurrency as usize));
        let http = authed_client(&cred);
        let node_task = tokio::spawn(ws_loop(cfg, cred, http, sem));

        let st = state.clone();
        wait_until("ws registration", Duration::from_secs(10), || async {
            st.ws_registry.connection_count().await == 1
        })
        .await;

        let task_id = create_task(&base, &token).await;
        wait_until("attempt running", Duration::from_secs(15), || async {
            let Ok(r) = reqwest::Client::new()
                .get(format!("{base}/v1/tasks/{task_id}"))
                .header("authorization", format!("Bearer {token}"))
                .send()
                .await
            else {
                return false;
            };
            r.text().await.unwrap_or_default().contains("\"running\"")
        })
        .await;

        // Kill the CP mid-attempt; the adapter keeps working for ~4 s.
        server1.abort();
        let _ = server1.await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let sock2 = tokio::net::TcpSocket::new_v4().unwrap();
        sock2.set_reuseaddr(true).unwrap();
        sock2.bind(addr).unwrap();
        let listener2 = sock2.listen(64).unwrap();
        let st2 = state.clone();
        let _server2 = tokio::spawn(async move {
            let _ = axum::serve(listener2, build_router(st2)).await;
        });

        // Attempt completes and all events land after the CP is back.
        wait_until(
            "task succeeded after CP restart",
            Duration::from_secs(60),
            || async {
                let Ok(r) = reqwest::Client::new()
                    .get(format!("{base}/v1/tasks/{task_id}"))
                    .header("authorization", format!("Bearer {token}"))
                    .send()
                    .await
                else {
                    return false;
                };
                r.text().await.unwrap_or_default().contains("\"succeeded\"")
            },
        )
        .await;
        let events: serde_json::Value = reqwest::Client::new()
            .get(format!("{base}/v1/tasks/{task_id}/events"))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let count = events.as_array().map(|a| a.len()).unwrap_or(0);
        assert!(
            count >= 10,
            "events lost across the CP outage: got {count}, want >= 10"
        );

        node_task.abort();
        std::env::set_var("PATH", old_path);
    }
}
