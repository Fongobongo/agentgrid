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

use crate::config::{Config, Endpoints, SavedCredential};
use crate::polling::{dispatch_batch, poll_loop_inner};

/// Consecutive WS connection failures tolerated before `auto` transport runs
/// the poll fallback window.
const MAX_CONSECUTIVE_WS_FAILURES: usize = 3;
/// Slot heartbeat interval on the WS channel (the HTTP heartbeat still runs).
const WS_HEARTBEAT_INTERVAL_SECS: u64 = 30;
/// Bound on a single heartbeat write — a full send buffer must fail the
/// session, not park it.
const WS_SEND_TIMEOUT_SECS: u64 = 15;
/// No inbound frame (the CP pings every ~15 s) for this long ⇒ half-open
/// connection.
///
/// The CP's `PING_INTERVAL` is 15 s, so this is three missed pings. It used to
/// be 90 s — six missed pings, 3× *our* 30 s heartbeat rather than the peer's
/// cadence — and that number was the single largest cost in a failover: a
/// SIGSTOPped leader keeps the TCP socket open and answers nothing, so the node
/// sat on a dead session for 90 s before even trying the new leader (see
/// `run-ha-partition.sh`). Three missed pings is the usual bound; anything
/// tighter starts treating a momentarily busy CP as dead.
const WS_READ_IDLE_SECS: u64 = 45;
/// Bound on the upgrade itself (TCP connect + HTTP handshake). A frozen leader
/// accepts the connection and never answers, so this is the cost of *trying*
/// the dead host before rotating to the next replica: 10 s keeps WAN tolerance
/// while halving the old 15 s.
const WS_HANDSHAKE_TIMEOUT_SECS: u64 = 10;

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

/// A failed connect attempt, plus whether it already moved the daemon onto a
/// hinted leader. The reconnect loops must not rotate after that: rotating
/// steps *off* the leader they were just told about, so the failover never
/// lands (see `next_after_failure`).
struct ConnectError {
    error: anyhow::Error,
    followed_hint: bool,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

/// Connect and send the `Hello` handshake; returns the open socket.
/// The handshake is time-bounded (10 s connect, 10 s overall): an accepting-
/// but-silent CP used to park the WS session setup forever.
async fn connect_once(cfg: &Config, cred: &SavedCredential) -> Result<WsStream, ConnectError> {
    let mut followed_hint = false;
    connect_inner(cfg, cred, &mut followed_hint)
        .await
        .map_err(|error| ConnectError {
            error,
            followed_hint,
        })
}

async fn connect_inner(
    cfg: &Config,
    cred: &SavedCredential,
    followed_hint: &mut bool,
) -> Result<WsStream> {
    let url = format!("{}/v1/node/ws", ws_url(&cfg.base()));
    let mut req = url.into_client_request()?;
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {}", cred.credential).parse()?,
    );
    let connected = tokio::time::timeout(
        std::time::Duration::from_secs(WS_HANDSHAKE_TIMEOUT_SECS),
        tokio_tungstenite::connect_async(req),
    )
    .await
    .map_err(|_| anyhow::anyhow!("ws handshake timed out"))?;
    let (mut sock, _resp) = match connected {
        Ok(ok) => ok,
        Err(e) => {
            // A standby answers 503 with the leader hint; follow it (only for
            // configured replicas) so the next attempt goes to the leader.
            if let tokio_tungstenite::tungstenite::Error::Http(resp) = &e {
                let hint = resp
                    .headers()
                    .get("x-agentgrid-leader")
                    .and_then(|v| v.to_str().ok());
                *followed_hint = cfg.endpoints.follow_hint(hint);
                if *followed_hint {
                    tracing::info!("ws: following leader hint to {}", cfg.base());
                }
            }
            return Err(e.into());
        }
    };
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

/// Why a connected session ended. `Demoted` means the CP lost its leader
/// lease and closed every WS with 4004: reconnect immediately, not after
/// backoff, and try the next configured replica.
enum SessionEnd {
    Closed,
    Demoted,
}

/// Run one connected session until the socket closes or errors.
async fn run_session(
    mut sock: WsStream,
    cfg: &Config,
    client: &Client,
    sem: &Arc<Semaphore>,
    cred: &SavedCredential,
) -> Result<SessionEnd> {
    tracing::info!("ws control channel connected to {}", cfg.base());
    let mut hb = tokio::time::interval(Duration::from_secs(WS_HEARTBEAT_INTERVAL_SECS));
    hb.tick().await; // consume the immediate first tick
                     // Last inbound frame (ping/pong/data). Checked below per iteration: the
                     // timeout cannot live inside select! — every heartbeat tick would rebuild
                     // the future and silently reset the deadline, so the read-idle would
                     // never fire (the first version of this fix had exactly that bug).
    let mut last_read = tokio::time::Instant::now();
    loop {
        if last_read.elapsed() >= Duration::from_secs(WS_READ_IDLE_SECS) {
            // The CP pings every ~15 s, so 3× our heartbeat interval of
            // inbound silence means a half-open TCP (NAT drop, partition):
            // sock.next() would otherwise never resolve and Cancel/
            // ConfigUpdate would stall until the OS noticed. Fail the
            // session; the reconnect loop takes it from here.
            anyhow::bail!("ws read-idle timeout ({WS_READ_IDLE_SECS}s) — half-open connection?");
        }
        tokio::select! {
            _ = hb.tick() => {
                let msg = NodeWsMsg::Heartbeat { free_slots: sem.available_permits() as u32 };
                // A half-open peer with a full TCP send buffer used to park
                // this send (and the whole session) forever; bound it so the
                // reconnect loop can heal the session instead.
                tokio::time::timeout(
                    Duration::from_secs(WS_SEND_TIMEOUT_SECS),
                    sock.send(Message::Text(serde_json::to_string(&msg)?.into())),
                )
                .await
                .map_err(|_| anyhow::anyhow!("ws heartbeat send timed out"))??;
            }
            msg = sock.next() => {
                let Some(msg) = msg else { return Ok(SessionEnd::Closed) };
                last_read = tokio::time::Instant::now();
                match msg? {
                    Message::Text(t) => handle_msg(&t, cfg, client, sem, &mut sock, cred).await?,
                    Message::Ping(p) => sock.send(Message::Pong(p)).await?,
                    Message::Close(cf) => {
                        tracing::info!("ws closed by control plane: {cf:?}");
                        let demoted = cf
                            .as_ref()
                            .map(|c| u16::from(c.code) == agentgrid_common::ws::WS_CLOSE_DEMOTED)
                            .unwrap_or(false);
                        return Ok(if demoted {
                            SessionEnd::Demoted
                        } else {
                            SessionEnd::Closed
                        });
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
            // Audit ND-5: spawned, not awaited — dispatch_batch waits on the
            // concurrency semaphore per assignment, and awaiting it inside
            // the message handler stalls the whole session (no pings
            // answered, no Cancel/ConfigUpdate processing) whenever the CP
            // over-subscribes the node, flapping the connection into the
            // same stall after each reconnect.
            let cfg2 = cfg.clone();
            let client2 = client.clone();
            let sem2 = sem.clone();
            tokio::spawn(async move {
                if let Err(e) = dispatch_batch(&cfg2, &client2, &sem2, assignments).await {
                    tracing::error!("ws dispatch_batch failed: {e}");
                }
            });
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

/// Next endpoint to try after a failed connect, or `None` when the daemon
/// should stay where it is.
///
/// A failure that already followed a `x-agentgrid-leader` hint must not rotate:
/// `follow_hint` moved `current` onto the leader, and `advance()` would
/// immediately step off it onto a standby — so the very hint that exists to
/// shorten failover was being thrown away, and a node kept retrying standbys
/// (and the frozen old leader) until the poll fallback happened to land on the
/// new leader minutes later. `heartbeat` and `poll` already guard this.
fn next_after_failure(endpoints: &Endpoints, followed_hint: bool) -> Option<String> {
    if followed_hint || endpoints.urls().len() < 2 {
        return None;
    }
    Some(endpoints.advance())
}

/// WS transport: reconnect with exponential backoff (1s → 60s) forever, so a
/// CP restart or network drop heals itself without manual intervention.
pub async fn ws_loop<F: Fn() -> Client>(
    cfg: Config,
    cred: SavedCredential,
    client: Client,
    sem: Arc<Semaphore>,
    mk_client: F,
) -> Result<()> {
    let _ = mk_client; // poll-fallback failover hook lives on the Poll path
    let mut backoff = Duration::from_secs(1);
    loop {
        match connect_once(&cfg, &cred).await {
            Ok(sock) => {
                backoff = Duration::from_secs(1);
                match run_session(sock, &cfg, &client, &sem, &cred).await {
                    Ok(SessionEnd::Demoted) => {
                        // Leader lost its lease: reconnect at once and try the
                        // next replica; failover is in progress.
                        cfg.endpoints.advance();
                        tracing::info!("ws: demoted, reconnecting to {}", cfg.base());
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                    Ok(SessionEnd::Closed) => {}
                    Err(e) => tracing::warn!("ws session ended: {e}"),
                }
            }
            Err(e) => {
                tracing::warn!("ws connect failed: {e}");
                if let Some(next) = next_after_failure(&cfg.endpoints, e.followed_hint) {
                    tracing::warn!("ws: rotating control-plane endpoint to {next}");
                }
            }
        }
        tracing::info!("ws reconnect in {backoff:?}");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// Auto transport: prefer WS; after `MAX_CONSECUTIVE_WS_FAILURES` failed
/// connects, run long polling for the current backoff window, then retry WS.
/// A mid-session drop (CP restart, network loss) reconnects promptly.
pub async fn auto_loop<F: Fn() -> Client + Clone>(
    cfg: Config,
    cred: SavedCredential,
    client: Client,
    sem: Arc<Semaphore>,
    mk_client: F,
) -> Result<()> {
    let mut backoff = Duration::from_secs(1);
    let mut consecutive_failures = 0usize;
    loop {
        match connect_once(&cfg, &cred).await {
            Ok(sock) => {
                consecutive_failures = 0;
                backoff = Duration::from_secs(1);
                match run_session(sock, &cfg, &client, &sem, &cred).await {
                    Ok(SessionEnd::Demoted) => {
                        cfg.endpoints.advance();
                        tracing::info!("ws: demoted, reconnecting to {}", cfg.base());
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Ok(SessionEnd::Closed) => {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    Err(e) => {
                        tracing::warn!("ws session ended: {e}");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
            Err(e) => {
                consecutive_failures += 1;
                tracing::warn!("ws connect failed ({consecutive_failures}): {e}");
                if let Some(next) = next_after_failure(&cfg.endpoints, e.followed_hint) {
                    tracing::warn!("ws: rotating control-plane endpoint to {next}");
                }
                if consecutive_failures >= MAX_CONSECUTIVE_WS_FAILURES {
                    tracing::info!("falling back to poll transport for {backoff:?}");
                    poll_loop_inner(
                        cfg.clone(),
                        client.clone(),
                        sem.clone(),
                        cred.node_id.clone(),
                        Some(backoff),
                        mk_client.clone(),
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

// Transport-agnostic: runs on every platform, unlike the process-level tests
// below.
#[cfg(test)]
mod failover_tests {
    use super::*;

    fn endpoints() -> Arc<Endpoints> {
        Endpoints::new(vec![
            "http://127.0.0.1:17921".into(),
            "http://127.0.0.1:17922".into(),
            "http://127.0.0.1:17923".into(),
        ])
    }

    #[test]
    fn a_followed_hint_is_not_rotated_away_from() {
        let e = endpoints();
        assert!(e.follow_hint(Some("http://127.0.0.1:17922")));
        assert_eq!(e.current(), "http://127.0.0.1:17922");
        // The regression: rotating here moved `current` to 17923 (a standby),
        // so the daemon never retried the leader the hint pointed at.
        assert_eq!(next_after_failure(&e, true), None);
        assert_eq!(e.current(), "http://127.0.0.1:17922");
    }

    #[test]
    fn a_failure_without_a_hint_still_rotates() {
        let e = endpoints();
        assert_eq!(
            next_after_failure(&e, false).as_deref(),
            Some("http://127.0.0.1:17922")
        );
        assert_eq!(e.current(), "http://127.0.0.1:17922");
    }

    #[test]
    fn a_single_endpoint_never_rotates() {
        let e = Endpoints::new(vec!["http://127.0.0.1:17921".into()]);
        assert_eq!(next_after_failure(&e, false), None);
    }
}

// The ws integration tests below boot a sh-script mock adapter and assert
// POSIX process behaviour — unix-only (matches the deployment target).
#[cfg(all(test, unix))]
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
            endpoints: Endpoints::new(vec![server.to_string()]),
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
            github_token: None,
            proxies: std::sync::Arc::new(crate::proxy::ProxyPool::new(vec![])),
            managed_adapter_env: Default::default(),
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
    #[cfg(unix)]
    async fn ws_node_survives_cp_restart() {
        use std::os::unix::fs::PermissionsExt;

        let _path_guard = crate::TEST_ENV_LOCK.lock().await;
        // Full attempts below drive the process-global config-error streak
        // counter; hold it so the threshold unit test keeps exact counts.
        let _counter_guard = crate::config_error::CONFIG_COUNTER_LOCK.lock().await;

        // Self-contained `adapter-mock` (JSON-line protocol): the test used to
        // rely on cargo-test's target/debug landing in PATH (or on a sibling
        // test's fixture leaking through a process-global PATH mutation —
        // racy under the parallel test harness). A missing binary turns the
        // attempt `infrastructure_failed` without ever reaching `running`,
        // which is exactly the observed timeout. Install our own, with a 2 s
        // sleep so the `running` window is wide enough for the poller to
        // observe it (an instant-exit mock finishes the whole task in ~24 ms,
        // racing the 50 ms polling loop — the flake of issue #61).
        let dir = std::env::temp_dir().join(format!(
            "ag-ws-reg-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let script = bin_dir.join("adapter-mock");
        std::fs::write(
            &script,
            "#!/bin/sh\necho '{\"type\":\"log\",\"payload\":{\"text\":\"hi\"}}'\n\
             sleep 2\n\
             echo '{\"type\":\"result\",\"payload\":{\"exit_code\":0,\"text\":\"done\"}}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", bin_dir.display()));

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
        let node_task = tokio::spawn(ws_loop(cfg, cred, http, sem, Client::new));

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
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut detail = String::new();
        while Instant::now() < deadline {
            if let Ok(r) = reqwest::Client::new()
                .get(format!("{base}/v1/tasks/{task_id}"))
                .header("authorization", format!("Bearer {token}"))
                .send()
                .await
            {
                let body = r.text().await.unwrap_or_default();
                if body.contains("\"running\"") {
                    detail.clear();
                    break;
                }
                detail = body;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if !detail.is_empty() {
            panic!(
                "timed out waiting for: task running via ws push\n\
                 task detail: {detail}\n\
                 ws connections: {} (expected 1)",
                state.ws_registry.connection_count().await
            );
        }

        node_task.abort();
        std::env::set_var("PATH", old_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Plan 0.3 2.4 failure injection: kill the CP mid-attempt. The attempt
    /// must complete once the CP is back, and its events must all land
    /// (durable event outbox + idempotent ingest + bounded retry).
    /// (Unix-only: sh-script mock adapter + chmod 0755.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[cfg(unix)]
    async fn ws_attempt_survives_cp_kill_midflight() {
        use std::os::unix::fs::PermissionsExt;

        let _path_guard = crate::TEST_ENV_LOCK.lock().await;
        let _counter_guard = crate::config_error::CONFIG_COUNTER_LOCK.lock().await;

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
        let node_task = tokio::spawn(ws_loop(cfg, cred, http, sem, Client::new));

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
        // The task flips to succeeded on the completion record; the tail of
        // the event stream may still be redriving from the outbox (bounded
        // retry backoff after the reconnect). Poll until the full batch
        // lands instead of counting once — a single count raced the redrive
        // and flaked as "events lost" on loaded CI runners.
        wait_until(
            "all events landed after CP restart",
            Duration::from_secs(30),
            || async {
                let Ok(r) = reqwest::Client::new()
                    .get(format!("{base}/v1/tasks/{task_id}/events"))
                    .header("authorization", format!("Bearer {token}"))
                    .send()
                    .await
                else {
                    return false;
                };
                let events: serde_json::Value = r.json().await.unwrap_or_default();
                events.as_array().is_some_and(|a| a.len() >= 10)
            },
        )
        .await;

        node_task.abort();
        std::env::set_var("PATH", old_path);
    }
}
