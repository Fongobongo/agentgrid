//! Stateless HA edge: forwards HTTP, SSE and the node WebSocket to the current
//! control-plane leader.
//!
//! The edge holds no state beyond a short leader cache. It discovers the
//! leader with `GET /v1/ha/status` (authenticated by the cluster token), sends
//! every client request there unchanged, and retries once after a `503
//! not_leader` or a connect failure. A leaderless cluster answers `503` with
//! `Retry-After`; the edge itself never accepts writes.
//!
//! This is an alternative to a VIP/DNS name. The control-plane's own `503`
//! leader hint remains the client-side failover path when no edge is deployed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ws::{
    CloseFrame as ClientClose, Message as ClientMessage, WebSocket, WebSocketUpgrade,
};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as UpstreamMessage;

const HA_TOKEN_HEADER: &str = "x-agentgrid-ha-token";
const LEADER_TTL: Duration = Duration::from_secs(2);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);
/// The proxy buffers a request body once so a `503 not_leader` retry is safe.
/// Matches the control-plane artifact ceiling (50 MiB) with headroom.
const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;

#[derive(Parser)]
#[command(name = "agentgrid-cp-edge", version, about)]
struct Args {
    /// Listen address, e.g. 0.0.0.0:7900.
    #[arg(long, env = "AGENTGRID_EDGE_LISTEN", default_value = "0.0.0.0:7900")]
    listen: String,
    /// Comma-separated control-plane base URLs.
    #[arg(long, env = "AGENTGRID_EDGE_UPSTREAMS")]
    upstreams: String,
    /// HA cluster token (also AGENTGRID_HA_CLUSTER_TOKEN).
    #[arg(long, env = "AGENTGRID_HA_CLUSTER_TOKEN")]
    cluster_token: String,
    /// Seconds to wait for an upstream to start answering. Bounds only the
    /// response head, so SSE and artifact downloads keep streaming. A leader
    /// that stops answering (partition, SIGSTOP, long GC pause) must become a
    /// retry on the new leader, not a client request that hangs forever.
    #[arg(
        long,
        env = "AGENTGRID_EDGE_UPSTREAM_TIMEOUT_SECS",
        default_value_t = 10
    )]
    upstream_timeout_secs: u64,
}

#[derive(Deserialize)]
struct HaStatus {
    role: String,
}

#[derive(Default)]
struct Metrics {
    requests_total: AtomicU64,
    leader_discoveries_total: AtomicU64,
    leaderless_total: AtomicU64,
    retries_total: AtomicU64,
    upstream_errors_total: AtomicU64,
    upstream_timeouts_total: AtomicU64,
    ws_bridges_total: AtomicU64,
}

struct Edge {
    upstreams: Vec<String>,
    token: String,
    http: reqwest::Client,
    /// Deadline for the response head of a proxied request.
    upstream_timeout: Duration,
    leader: tokio::sync::RwLock<Option<CachedLeader>>,
    metrics: Metrics,
}

struct CachedLeader {
    url: String,
    at: Instant,
}

impl Edge {
    fn new(
        upstreams: Vec<String>,
        token: String,
        upstream_timeout: Duration,
    ) -> anyhow::Result<Arc<Self>> {
        if upstreams.is_empty() {
            anyhow::bail!("AGENTGRID_EDGE_UPSTREAMS must list at least one control plane");
        }
        let http = reqwest::Client::builder()
            // Never replay a client request (or the cluster token) to a
            // redirect target chosen by an upstream.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Arc::new(Self {
            upstreams,
            token,
            http,
            upstream_timeout,
            leader: tokio::sync::RwLock::new(None),
            metrics: Metrics::default(),
        }))
    }

    async fn leader(&self) -> Option<String> {
        if let Some(cached) = self.leader.read().await.as_ref() {
            if cached.at.elapsed() < LEADER_TTL {
                return Some(cached.url.clone());
            }
        }
        let found = self.discover().await;
        if found.is_some() {
            self.metrics
                .leader_discoveries_total
                .fetch_add(1, Ordering::Relaxed);
        }
        *self.leader.write().await = found.clone().map(|url| CachedLeader {
            url,
            at: Instant::now(),
        });
        found
    }

    async fn discover(&self) -> Option<String> {
        for base in &self.upstreams {
            let url = format!("{}/v1/ha/status", base.trim_end_matches('/'));
            let result = self
                .http
                .get(&url)
                .header(HA_TOKEN_HEADER, &self.token)
                .timeout(DISCOVERY_TIMEOUT)
                .send()
                .await;
            let Ok(resp) = result else {
                tracing::debug!(upstream = %base, "discovery: no answer");
                continue;
            };
            if !resp.status().is_success() {
                tracing::debug!(
                    upstream = %base,
                    status = resp.status().as_u16(),
                    "discovery: non-success status"
                );
                continue;
            }
            let Ok(status) = resp.json::<HaStatus>().await else {
                tracing::debug!(upstream = %base, "discovery: unparsable status body");
                continue;
            };
            if status.role == "leader" {
                return Some(base.trim_end_matches('/').to_string());
            }
            tracing::debug!(upstream = %base, role = %status.role, "discovery: not the leader");
        }
        None
    }

    async fn invalidate(&self) {
        *self.leader.write().await = None;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let upstreams: Vec<String> = args
        .upstreams
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .collect();
    let edge = Edge::new(
        upstreams,
        args.cluster_token,
        Duration::from_secs(args.upstream_timeout_secs.max(1)),
    )?;
    let listen: std::net::SocketAddr = args.listen.parse()?;
    let app = Router::new()
        .route("/health/live", get(|| async { "ok" }))
        .route("/health/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/v1/node/ws", get(node_ws))
        .fallback(proxy)
        .with_state(edge);
    tracing::info!(%listen, "agentgrid edge listening");
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ready(State(edge): State<Arc<Edge>>) -> Response {
    match edge.leader().await {
        Some(_) => (StatusCode::OK, "leader reachable").into_response(),
        None => no_leader(),
    }
}

fn no_leader() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "2")],
        "no control-plane leader",
    )
        .into_response()
}

async fn metrics(State(edge): State<Arc<Edge>>) -> Response {
    let lines = format!(
        "# TYPE agentgrid_edge_requests_total counter\n\
        agentgrid_edge_requests_total {requests_total}\n\
        # TYPE agentgrid_edge_leader_discoveries_total counter\n\
        agentgrid_edge_leader_discoveries_total {leader_discoveries_total}\n\
        # TYPE agentgrid_edge_leaderless_total counter\n\
        agentgrid_edge_leaderless_total {leaderless_total}\n\
        # TYPE agentgrid_edge_retries_total counter\n\
        agentgrid_edge_retries_total {retries_total}\n\
        # TYPE agentgrid_edge_upstream_errors_total counter\n\
        agentgrid_edge_upstream_errors_total {upstream_errors_total}\n\
        # TYPE agentgrid_edge_upstream_timeouts_total counter\n\
        agentgrid_edge_upstream_timeouts_total {upstream_timeouts_total}\n\
        # TYPE agentgrid_edge_ws_bridges_total counter\n\
        agentgrid_edge_ws_bridges_total {ws_bridges_total}\n\
        # HELP agentgrid_edge_requests_total Total client requests handled by the edge.\n\
        # HELP agentgrid_edge_leader_discoveries_total Successful leader discoveries via /v1/ha/status.\n\
        # HELP agentgrid_edge_leaderless_total Requests answered 503 because no leader was reachable.\n\
        # HELP agentgrid_edge_retries_total Proxied requests retried once after 503/not_leader or connect error.\n\
        # HELP agentgrid_edge_upstream_errors_total Proxied requests that failed all upstream attempts.\n\
        # HELP agentgrid_edge_upstream_timeouts_total Proxied requests whose upstream did not answer in time.\n\
        # HELP agentgrid_edge_ws_bridges_total Total WebSocket bridges established.\n",
        requests_total = edge.metrics.requests_total.load(Ordering::Relaxed),
        leader_discoveries_total = edge.metrics.leader_discoveries_total.load(Ordering::Relaxed),
        leaderless_total = edge.metrics.leaderless_total.load(Ordering::Relaxed),
        retries_total = edge.metrics.retries_total.load(Ordering::Relaxed),
        upstream_errors_total = edge.metrics.upstream_errors_total.load(Ordering::Relaxed),
        upstream_timeouts_total = edge
            .metrics
            .upstream_timeouts_total
            .load(Ordering::Relaxed),
        ws_bridges_total = edge.metrics.ws_bridges_total.load(Ordering::Relaxed),
    );
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        lines,
    )
        .into_response()
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

async fn proxy(State(edge): State<Arc<Edge>>, req: Request) -> Response {
    edge.metrics.requests_total.fetch_add(1, Ordering::Relaxed);
    let (parts, body) = req.into_parts();
    let limit = axum::body::to_bytes(body, MAX_REQUEST_BODY).await;
    let bytes = match limit {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
    };
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let mut retried = false;
    loop {
        let Some(base) = edge.leader().await else {
            edge.metrics
                .leaderless_total
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                upstreams = ?edge.upstreams,
                "no control-plane reports itself leader; answering 503"
            );
            return no_leader();
        };
        let url = format!("{base}{path}");
        let mut rb = edge.http.request(parts.method.clone(), url);
        for (name, value) in &parts.headers {
            if !is_hop_by_hop(name) {
                rb = rb.header(name, value);
            }
        }
        // Bound the wait for the response *head* only. `send()` resolves once
        // the headers are parsed, so a long SSE stream or a slow artifact
        // download is unaffected — but an upstream that accepted the
        // connection and then went silent (SIGSTOP, partition, stuck leader)
        // surfaces as a timeout here and takes the retry path below instead of
        // pinning the client request forever.
        let sent = match tokio::time::timeout(edge.upstream_timeout, rb.body(bytes.clone()).send())
            .await
        {
            Ok(sent) => {
                sent.map_err(|e| tracing::debug!(error = %e, "edge upstream request failed"))
            }
            Err(_) => {
                edge.metrics
                    .upstream_timeouts_total
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    upstream = %base,
                    timeout_secs = edge.upstream_timeout.as_secs(),
                    "edge upstream did not answer in time"
                );
                Err(())
            }
        };
        match sent {
            Ok(resp) => {
                // A standby answers with the role header; the leader may have
                // moved between discovery and forwarding. Retry once, after a
                // fresh discovery, because the request was never processed.
                if resp.status() == StatusCode::SERVICE_UNAVAILABLE
                    && resp.headers().contains_key("x-agentgrid-role")
                    && !retried
                {
                    retried = true;
                    edge.metrics.retries_total.fetch_add(1, Ordering::Relaxed);
                    edge.invalidate().await;
                    continue;
                }
                return forward_response(resp);
            }
            Err(()) => {
                if !retried {
                    retried = true;
                    edge.metrics.retries_total.fetch_add(1, Ordering::Relaxed);
                    edge.invalidate().await;
                    continue;
                }
                edge.metrics
                    .upstream_errors_total
                    .fetch_add(1, Ordering::Relaxed);
                return (StatusCode::BAD_GATEWAY, "control-plane unreachable").into_response();
            }
        }
    }
}

fn forward_response(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let mut headers = HeaderMap::new();
    for (name, value) in resp.headers() {
        if !is_hop_by_hop(name) {
            headers.insert(name.clone(), value.clone());
        }
    }
    // Stream both SSE and large artifact downloads; response headers are
    // copied verbatim (including content-type and content-encoding).
    let stream = resp.bytes_stream();
    let body = Body::from_stream(stream);
    let mut out = Response::new(body);
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

async fn node_ws(ws: WebSocketUpgrade, State(edge): State<Arc<Edge>>, req: Request) -> Response {
    let Some(base) = edge.leader().await else {
        return no_leader();
    };
    let edge2 = Arc::clone(&edge);
    let url = format!(
        "{}/v1/node/ws",
        base.replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1)
    );
    edge2
        .metrics
        .ws_bridges_total
        .fetch_add(1, Ordering::Relaxed);
    let headers = req.headers().clone();
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = bridge_ws(socket, &url, &headers).await {
            tracing::warn!(error = %e, %url, "edge websocket bridge ended");
        }
    })
}

async fn bridge_ws(client: WebSocket, url: &str, headers: &HeaderMap) -> anyhow::Result<()> {
    let mut upstream_req = url.into_client_request()?;
    for (name, value) in headers {
        if is_hop_by_hop(name)
            || matches!(
                name.as_str(),
                "sec-websocket-key" | "sec-websocket-version" | "sec-websocket-extensions"
            )
        {
            continue;
        }
        upstream_req
            .headers_mut()
            .insert(name.clone(), value.clone());
    }
    let (upstream, _) = tokio_tungstenite::connect_async(upstream_req).await?;
    let (mut client_tx, mut client_rx) = client.split();
    let (mut up_tx, mut up_rx) = upstream.split();
    loop {
        tokio::select! {
            msg = client_rx.next() => match msg {
                Some(Ok(ClientMessage::Text(text))) => {
                    if up_tx.send(UpstreamMessage::Text(text.to_string().into())).await.is_err() { break; }
                }
                Some(Ok(ClientMessage::Binary(bytes))) => {
                    if up_tx.send(UpstreamMessage::Binary(bytes)).await.is_err() { break; }
                }
                Some(Ok(ClientMessage::Ping(bytes))) => {
                    if up_tx.send(UpstreamMessage::Ping(bytes)).await.is_err() { break; }
                }
                Some(Ok(ClientMessage::Pong(bytes))) => {
                    if up_tx.send(UpstreamMessage::Pong(bytes)).await.is_err() { break; }
                }
                Some(Ok(ClientMessage::Close(frame))) => {
                    let frame = frame.map(|ClientClose { code, reason }| {
                        tokio_tungstenite::tungstenite::protocol::CloseFrame {
                            code: code.into(),
                            reason: reason.to_string().into(),
                        }
                    });
                    let _ = up_tx.send(UpstreamMessage::Close(frame)).await;
                    break;
                }
                _ => break,
            },
            msg = up_rx.next() => match msg {
                Some(Ok(UpstreamMessage::Text(text))) => {
                    if client_tx.send(ClientMessage::Text(text.to_string().into())).await.is_err() { break; }
                }
                Some(Ok(UpstreamMessage::Binary(bytes))) => {
                    if client_tx.send(ClientMessage::Binary(bytes)).await.is_err() { break; }
                }
                Some(Ok(UpstreamMessage::Ping(bytes))) => {
                    if client_tx.send(ClientMessage::Ping(bytes)).await.is_err() { break; }
                }
                Some(Ok(UpstreamMessage::Pong(bytes))) => {
                    if client_tx.send(ClientMessage::Pong(bytes)).await.is_err() { break; }
                }
                Some(Ok(UpstreamMessage::Close(frame))) => {
                    let frame = frame.map(|frame| ClientClose {
                        code: frame.code.into(),
                        reason: frame.reason.to_string().into(),
                    });
                    let _ = client_tx.send(ClientMessage::Close(frame)).await;
                    break;
                }
                _ => break,
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use axum::Json;

    /// One fake control plane: `/v1/ha/status` reports `role`, everything else
    /// answers `proxy` (a 503 also demotes the fake, like a real old leader).
    #[derive(Clone)]
    struct Upstream {
        role: Arc<Mutex<&'static str>>,
        proxy: StatusCode,
        hits: Arc<AtomicU64>,
    }

    async fn start_upstream(role: &'static str, proxy: StatusCode) -> (String, Upstream) {
        let up = Upstream {
            role: Arc::new(Mutex::new(role)),
            proxy,
            hits: Arc::new(AtomicU64::new(0)),
        };
        let app = Router::new()
            .route("/v1/ha/status", get(ha_status))
            .fallback(proxied)
            .with_state(up.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let addr = listener.local_addr().expect("upstream addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), up)
    }

    async fn ha_status(State(up): State<Upstream>) -> Json<serde_json::Value> {
        let role = *up.role.lock().expect("role lock");
        Json(serde_json::json!({ "role": role }))
    }

    async fn proxied(State(up): State<Upstream>) -> Response {
        up.hits.fetch_add(1, Ordering::Relaxed);
        if up.proxy == StatusCode::SERVICE_UNAVAILABLE {
            // A leader that lost the election reports standby from now on.
            *up.role.lock().expect("role lock") = "standby";
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [(HeaderName::from_static("x-agentgrid-role"), "standby")],
                "not leader",
            )
                .into_response()
        } else {
            (StatusCode::OK, "ok").into_response()
        }
    }

    fn edge(urls: &[&str]) -> Arc<Edge> {
        edge_with_timeout(urls, Duration::from_secs(10))
    }

    fn edge_with_timeout(urls: &[&str], timeout: Duration) -> Arc<Edge> {
        Edge::new(
            urls.iter().map(|u| (*u).to_string()).collect(),
            "token".into(),
            timeout,
        )
        .expect("edge")
    }

    /// An upstream that answers `/v1/ha/status` (so discovery finds a leader)
    /// but accepts every other request and then never answers — what a
    /// SIGSTOPped or partitioned leader looks like from the edge.
    ///
    /// `demote_after_first_status` makes it report `standby` from the second
    /// status call on, so a rediscovery after `invalidate()` moves to the
    /// healthy peer instead of picking the silent node again.
    async fn start_silent_upstream(demote_after_first_status: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind silent upstream");
        let addr = listener.local_addr().expect("silent upstream addr");
        let status_hits = Arc::new(AtomicU64::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let status_hits = Arc::clone(&status_hits);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    if head.contains("/v1/ha/status") {
                        let first = status_hits.fetch_add(1, Ordering::Relaxed) == 0;
                        let role = if demote_after_first_status && !first {
                            "standby"
                        } else {
                            "leader"
                        };
                        let body = format!(r#"{{"role":"{role}"}}"#);
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        return;
                    }
                    // Swallow the request and hold the socket open: the client
                    // sees a live connection with no response head.
                    tokio::time::sleep(Duration::from_secs(60)).await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn get_request(path: &str) -> Request {
        Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .expect("request")
    }

    #[tokio::test]
    async fn discovers_the_leader_among_upstreams() {
        let (standby, _a) = start_upstream("standby", StatusCode::OK).await;
        let (leader, _b) = start_upstream("leader", StatusCode::OK).await;
        let e = edge(&[&standby, &leader]);
        assert_eq!(e.leader().await.as_deref(), Some(leader.as_str()));
    }

    #[tokio::test]
    async fn unreachable_upstreams_are_leaderless() {
        let e = edge(&["http://127.0.0.1:1"]);
        assert!(e.leader().await.is_none());
    }

    #[tokio::test]
    async fn leaderless_cluster_answers_503_with_retry_after() {
        let (standby, _a) = start_upstream("standby", StatusCode::OK).await;
        let e = edge(&[&standby]);
        assert!(e.leader().await.is_none());

        let resp = proxy(State(e.clone()), get_request("/v1/tasks")).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers()
                .get(header::RETRY_AFTER)
                .expect("retry-after")
                .to_str()
                .expect("ascii"),
            "2"
        );
        assert_eq!(e.metrics.leaderless_total.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn proxy_retries_once_after_not_leader() {
        let (old, old_up) = start_upstream("leader", StatusCode::SERVICE_UNAVAILABLE).await;
        let (new, new_up) = start_upstream("leader", StatusCode::OK).await;
        let e = edge(&[&old, &new]);

        let resp = proxy(State(e.clone()), get_request("/v1/tasks")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(old_up.hits.load(Ordering::Relaxed), 1);
        assert_eq!(new_up.hits.load(Ordering::Relaxed), 1);
        assert_eq!(e.metrics.retries_total.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn proxy_retry_budget_is_a_single_attempt() {
        let (a, a_up) = start_upstream("leader", StatusCode::SERVICE_UNAVAILABLE).await;
        let (b, b_up) = start_upstream("leader", StatusCode::SERVICE_UNAVAILABLE).await;
        let e = edge(&[&a, &b]);

        let resp = proxy(State(e.clone()), get_request("/v1/tasks")).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        // First attempt plus exactly one retry: never a loop.
        let total = a_up.hits.load(Ordering::Relaxed) + b_up.hits.load(Ordering::Relaxed);
        assert_eq!(total, 2);
        assert_eq!(e.metrics.retries_total.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn silent_upstream_times_out_instead_of_hanging() {
        // The regression this guards: a leader that accepts the connection and
        // then stops answering (SIGSTOP / partition) used to pin the proxied
        // request forever, so the edge never failed over and the client hung
        // too. Now the head wait is bounded and the retry path runs.
        let silent = start_silent_upstream(true).await;
        let (fresh, fresh_up) = start_upstream("leader", StatusCode::OK).await;
        let e = edge_with_timeout(&[&silent, &fresh], Duration::from_millis(300));

        let started = Instant::now();
        let resp = proxy(State(e.clone()), get_request("/v1/tasks")).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "bounded, not hung"
        );
        // First attempt timed out, the retry landed on the healthy upstream.
        assert_eq!(e.metrics.upstream_timeouts_total.load(Ordering::Relaxed), 1);
        assert_eq!(e.metrics.retries_total.load(Ordering::Relaxed), 1);
        assert_eq!(fresh_up.hits.load(Ordering::Relaxed), 1);
        assert_eq!(e.metrics.upstream_errors_total.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn all_upstreams_silent_is_a_502_not_a_hang() {
        let a = start_silent_upstream(false).await;
        let b = start_silent_upstream(false).await;
        let e = edge_with_timeout(&[&a, &b], Duration::from_millis(200));

        let resp = proxy(State(e.clone()), get_request("/v1/tasks")).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(e.metrics.upstream_timeouts_total.load(Ordering::Relaxed), 2);
        assert_eq!(e.metrics.upstream_errors_total.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn proxied_response_keeps_upstream_headers() {
        let (leader, _a) = start_upstream("leader", StatusCode::OK).await;
        let e = edge(&[&leader]);
        let resp = proxy(State(e), get_request("/v1/tasks?state=queued")).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        for name in [
            "connection",
            "keep-alive",
            "transfer-encoding",
            "content-length",
            "upgrade",
            "host",
        ] {
            assert!(
                is_hop_by_hop(&HeaderName::from_static(name)),
                "{name} must not be forwarded"
            );
        }
        for name in ["authorization", "x-agentgrid-role", "content-type"] {
            assert!(
                !is_hop_by_hop(&HeaderName::from_static(name)),
                "{name} must be forwarded"
            );
        }
    }

    #[tokio::test]
    async fn metrics_endpoint_reports_counters() {
        let e = edge(&["http://127.0.0.1:1"]);
        e.metrics.requests_total.fetch_add(3, Ordering::Relaxed);
        let resp = metrics(State(e)).await;
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("metrics body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(text.contains("agentgrid_edge_requests_total 3"));
        assert!(text.contains("agentgrid_edge_leaderless_total 0"));
        assert!(text.contains("# HELP agentgrid_edge_ws_bridges_total"));
        assert!(text.contains("agentgrid_edge_upstream_timeouts_total 0"));
    }
}
