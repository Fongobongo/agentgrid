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
    ws_bridges_total: AtomicU64,
}

struct Edge {
    upstreams: Vec<String>,
    token: String,
    http: reqwest::Client,
    leader: tokio::sync::RwLock<Option<CachedLeader>>,
    metrics: Metrics,
}

struct CachedLeader {
    url: String,
    at: Instant,
}

impl Edge {
    fn new(upstreams: Vec<String>, token: String) -> anyhow::Result<Arc<Self>> {
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
                continue;
            };
            if !resp.status().is_success() {
                continue;
            }
            let Ok(status) = resp.json::<HaStatus>().await else {
                continue;
            };
            if status.role == "leader" {
                return Some(base.trim_end_matches('/').to_string());
            }
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
    let edge = Edge::new(upstreams, args.cluster_token)?;
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
        # TYPE agentgrid_edge_ws_bridges_total counter\n\
        agentgrid_edge_ws_bridges_total {ws_bridges_total}\n\
        # HELP agentgrid_edge_requests_total Total client requests handled by the edge.\n\
        # HELP agentgrid_edge_leader_discoveries_total Successful leader discoveries via /v1/ha/status.\n\
        # HELP agentgrid_edge_leaderless_total Requests answered 503 because no leader was reachable.\n\
        # HELP agentgrid_edge_retries_total Proxied requests retried once after 503/not_leader or connect error.\n\
        # HELP agentgrid_edge_upstream_errors_total Proxied requests that failed all upstream attempts.\n\
        # HELP agentgrid_edge_ws_bridges_total Total WebSocket bridges established.\n",
        requests_total = edge.metrics.requests_total.load(Ordering::Relaxed),
        leader_discoveries_total = edge.metrics.leader_discoveries_total.load(Ordering::Relaxed),
        leaderless_total = edge.metrics.leaderless_total.load(Ordering::Relaxed),
        retries_total = edge.metrics.retries_total.load(Ordering::Relaxed),
        upstream_errors_total = edge.metrics.upstream_errors_total.load(Ordering::Relaxed),
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
            return no_leader();
        };
        let url = format!("{base}{path}");
        let mut rb = edge.http.request(parts.method.clone(), url);
        for (name, value) in &parts.headers {
            if !is_hop_by_hop(name) {
                rb = rb.header(name, value);
            }
        }
        let sent = rb.body(bytes.clone()).send().await;
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
            Err(e) => {
                tracing::debug!(error = %e, "edge upstream request failed");
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
