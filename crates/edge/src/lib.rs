//! Forwarding core shared by the standalone edge and the node-hosted relay.
//!
//! The edge forwards HTTP, SSE and the node WebSocket to the current
//! control-plane leader. It holds no state beyond a short leader cache, sends
//! every client request upstream unchanged, and retries once after a
//! `503 not_leader` or a connect failure. A leaderless cluster answers `503`
//! with `Retry-After`; the forwarder itself never accepts writes.
//!
//! Two callers, one implementation:
//!
//! - `agentgrid-cp-edge` — a standalone process in front of an HA cluster. It
//!   finds the leader by asking `/v1/ha/status` with the cluster token
//!   ([`Upstream::Ha`]).
//! - `agentgrid-node-daemon` with `AGENTGRID_RELAY_LISTEN` — a node that can
//!   reach the control plane carries a peer's traffic ([`Upstream::Fixed`]).
//!   It needs no cluster token: the daemon already tracks the current
//!   control-plane endpoint and follows leader hints on its own transport, so
//!   the relay asks it for `base()` per request.
//!
//! The forwarder is not an open proxy: [`EdgeConfig::allowed_sources`] bounds
//! which peers may use it, and it only ever dials the upstream it was built
//! with — never a destination taken from the request.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ws::{
    CloseFrame as ClientClose, Message as ClientMessage, WebSocket, WebSocketUpgrade,
};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as UpstreamMessage;

const HA_TOKEN_HEADER: &str = "x-agentgrid-ha-token";
const LEADER_TTL: Duration = Duration::from_secs(2);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);
/// The forwarder buffers a request body once so a `503 not_leader` retry is
/// safe. Matches the control-plane artifact ceiling (50 MiB) with headroom.
const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;

/// Where the forwarder sends traffic.
pub enum Upstream {
    /// Standalone edge in front of an HA cluster: ask each replica
    /// `/v1/ha/status` (cluster token) and use the one that says `leader`.
    Ha(HaUpstream),
    /// Node-hosted relay: the caller already knows the control plane and keeps
    /// the answer current. Called once per request, so a leader move is picked
    /// up on the next request instead of after a TTL.
    Fixed(Arc<dyn Fn() -> String + Send + Sync>),
}

pub struct HaUpstream {
    pub upstreams: Vec<String>,
    pub token: String,
}

/// Everything needed to build a forwarder.
pub struct EdgeConfig {
    pub upstream: Upstream,
    /// Deadline for the response head of a forwarded request. SSE and artifact
    /// downloads keep streaming; this bounds only the wait for headers.
    pub upstream_timeout: Duration,
    /// Peer addresses allowed to use this forwarder. Empty means "any source",
    /// which is right for a standalone edge that is already behind the
    /// operator's network policy, and wrong for a node-hosted relay — the
    /// daemon warns when it starts one without a list.
    pub allowed_sources: Vec<Cidr>,
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
    rejected_sources_total: AtomicU64,
}

pub struct Edge {
    upstream: Upstream,
    http: reqwest::Client,
    upstream_timeout: Duration,
    allowed_sources: Vec<Cidr>,
    /// Only used by [`Upstream::Ha`]; the fixed variant has nothing to cache.
    leader: tokio::sync::RwLock<Option<CachedLeader>>,
    metrics: Metrics,
}

struct CachedLeader {
    url: String,
    at: Instant,
}

impl Edge {
    pub fn new(cfg: EdgeConfig) -> anyhow::Result<Arc<Self>> {
        if let Upstream::Ha(ha) = &cfg.upstream {
            if ha.upstreams.is_empty() {
                anyhow::bail!("an HA edge must list at least one control plane");
            }
        }
        let http = reqwest::Client::builder()
            // Never replay a client request (or the cluster token) to a
            // redirect target chosen by an upstream.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Arc::new(Self {
            upstream: cfg.upstream,
            http,
            upstream_timeout: cfg.upstream_timeout,
            allowed_sources: cfg.allowed_sources,
            leader: tokio::sync::RwLock::new(None),
            metrics: Metrics::default(),
        }))
    }

    /// Upstreams to report in logs and metrics.
    fn upstream_label(&self) -> Vec<String> {
        match &self.upstream {
            Upstream::Ha(ha) => ha.upstreams.clone(),
            Upstream::Fixed(f) => vec![f()],
        }
    }

    async fn leader(&self) -> Option<String> {
        match &self.upstream {
            // Asked every request on purpose: the daemon's `base()` already
            // moved if a leader hint arrived, and a cache here would serve a
            // stale replica for up to LEADER_TTL after that.
            Upstream::Fixed(base) => {
                let url = base();
                let url = url.trim_end_matches('/');
                if url.is_empty() {
                    None
                } else {
                    Some(url.to_string())
                }
            }
            Upstream::Ha(_) => {
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
        }
    }

    async fn discover(&self) -> Option<String> {
        let Upstream::Ha(ha) = &self.upstream else {
            return None;
        };
        for base in &ha.upstreams {
            let url = format!("{}/v1/ha/status", base.trim_end_matches('/'));
            let result = self
                .http
                .get(&url)
                .header(HA_TOKEN_HEADER, &ha.token)
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

    fn source_allowed(&self, addr: IpAddr) -> bool {
        self.allowed_sources.is_empty() || self.allowed_sources.iter().any(|c| c.contains(addr))
    }
}

/// Build the forwarder's routes. `/metrics`, `/health/*` and `/v1/node/ws` are
/// served here; everything else is forwarded.
pub fn router(edge: Arc<Edge>) -> Router {
    Router::new()
        .route("/health/live", get(|| async { "ok" }))
        .route("/health/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/v1/node/ws", get(node_ws))
        .fallback(proxy)
        .with_state(edge)
}

/// Serve the forwarder on an already-bound listener until it fails.
///
/// The caller binds, so a port that is already taken fails its own startup
/// loudly instead of surfacing as a task that quietly died: the node daemon
/// treats "relay configured but not listening" as fatal, and it can only do
/// that if the bind happens on its side.
pub async fn serve(edge: Arc<Edge>, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    let listen = listener.local_addr()?;
    // The allowlist layer needs its own handle: `router()` consumes one.
    let gate = Arc::clone(&edge);
    let app = router(edge).layer(middleware::from_fn_with_state(gate, guard_sources));
    tracing::info!(%listen, "forwarder listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Reject peers outside `allowed_sources` before any body is read.
async fn guard_sources(
    State(state): State<Arc<Edge>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if state.source_allowed(addr.ip()) {
        return next.run(req).await;
    }
    state
        .metrics
        .rejected_sources_total
        .fetch_add(1, Ordering::Relaxed);
    tracing::warn!(peer = %addr, "forwarder rejected a peer outside the allowlist");
    (StatusCode::FORBIDDEN, "peer not in the relay allowlist").into_response()
}

async fn ready(State(edge): State<Arc<Edge>>) -> Response {
    match edge.leader().await {
        Some(_) => (StatusCode::OK, "upstream reachable").into_response(),
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
        # TYPE agentgrid_edge_rejected_sources_total counter\n\
        agentgrid_edge_rejected_sources_total {rejected_sources_total}\n\
        # HELP agentgrid_edge_requests_total Total client requests handled by the forwarder.\n\
        # HELP agentgrid_edge_leader_discoveries_total Successful leader discoveries via /v1/ha/status.\n\
        # HELP agentgrid_edge_leaderless_total Requests answered 503 because no leader was reachable.\n\
        # HELP agentgrid_edge_retries_total Forwarded requests retried once after 503/not_leader or connect error.\n\
        # HELP agentgrid_edge_upstream_errors_total Forwarded requests that failed all upstream attempts.\n\
        # HELP agentgrid_edge_upstream_timeouts_total Forwarded requests whose upstream did not answer in time.\n\
        # HELP agentgrid_edge_ws_bridges_total Total WebSocket bridges established.\n\
        # HELP agentgrid_edge_rejected_sources_total Peers refused by the allowlist before any request was read.\n",
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
        rejected_sources_total = edge.metrics.rejected_sources_total.load(Ordering::Relaxed),
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
                upstreams = ?edge.upstream_label(),
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
                sent.map_err(|e| tracing::debug!(error = %e, "forwarder upstream request failed"))
            }
            Err(_) => {
                edge.metrics
                    .upstream_timeouts_total
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    upstream = %base,
                    timeout_secs = edge.upstream_timeout.as_secs(),
                    "forwarder upstream did not answer in time"
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
    let timeout = edge2.upstream_timeout;
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = bridge_ws(socket, &url, &headers, timeout).await {
            // The cached leader is the likeliest reason the upgrade failed;
            // drop it so this node's retry rediscovers instead of re-hanging.
            edge2.invalidate().await;
            tracing::warn!(error = %e, %url, "forwarder websocket bridge ended");
        }
    })
}

async fn bridge_ws(
    client: WebSocket,
    url: &str,
    headers: &HeaderMap,
    timeout: Duration,
) -> anyhow::Result<()> {
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
    // Bound the upgrade. A frozen leader accepts the TCP connection and never
    // answers the handshake, so an unbounded `connect_async` parks the node's
    // reconnect attempt forever: the node never re-registers and no task is
    // ever assigned. Timing out closes the client socket, and the node retries
    // through the forwarder, which rediscovers the new leader.
    let (upstream, _) =
        tokio::time::timeout(timeout, tokio_tungstenite::connect_async(upstream_req))
            .await
            .map_err(|_| {
                anyhow::anyhow!("upstream websocket upgrade timed out after {timeout:?}")
            })??;
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

/// An IPv4/IPv6 prefix, for bounding who may use a node-hosted relay.
///
/// Deliberately tiny: no dependency, no `0.0.0.0/0` special cases beyond the
/// arithmetic, and a bare address (`10.0.0.5`) is a `/32` or `/128` rather
/// than a parse error — that is what an operator writes for a single peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cidr {
    base: IpAddr,
    bits: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let s = s.trim();
        let (addr, bits) = match s.split_once('/') {
            Some((a, b)) => {
                let bits: u8 = b
                    .parse()
                    .map_err(|_| anyhow::anyhow!("{s:?}: prefix length is not a number"))?;
                (a, Some(bits))
            }
            None => (s, None),
        };
        let base: IpAddr = addr
            .parse()
            .map_err(|_| anyhow::anyhow!("{s:?}: not an IP address or CIDR prefix"))?;
        let max = match base {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        let bits = bits.unwrap_or(max);
        if bits > max {
            anyhow::bail!("{s:?}: prefix length {bits} exceeds {max} for this family");
        }
        Ok(Self { base, bits })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.base, ip) {
            (IpAddr::V4(base), IpAddr::V4(ip)) => {
                prefix_eq(u32::from(base), u32::from(ip), self.bits)
            }
            (IpAddr::V6(base), IpAddr::V6(ip)) => {
                prefix_eq(u128::from(base), u128::from(ip), self.bits)
            }
            // A v4 allowlist entry never matches a v6 peer: an operator who
            // wrote 10.0.0.0/8 did not mean ::ffff:10.0.0.1, and pretending
            // otherwise would widen the list silently.
            _ => false,
        }
    }
}

fn prefix_eq<T>(base: T, ip: T, bits: u8) -> bool
where
    T: Into<u128> + Copy,
{
    let width = std::mem::size_of::<T>() as u8 * 8;
    let base: u128 = base.into();
    let ip: u128 = ip.into();
    if bits == 0 {
        return true;
    }
    let shift = width - bits;
    (base >> shift) == (ip >> shift)
}

/// Parse a comma-separated allowlist. `None` or an empty string yields an empty
/// list, which the gate reads as "no restriction" — the caller decides whether
/// that is acceptable and says so in its own log.
pub fn parse_allowlist(spec: Option<&str>) -> anyhow::Result<Vec<Cidr>> {
    let Some(spec) = spec else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(Cidr::parse(part)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::sync::Mutex;

    use axum::Json;

    /// One fake control plane: `/v1/ha/status` reports `role`, everything else
    /// answers `proxy` (a 503 also demotes the fake, like a real old leader).
    #[derive(Clone)]
    struct Upstream0 {
        role: Arc<Mutex<&'static str>>,
        proxy: StatusCode,
        hits: Arc<AtomicU64>,
    }

    async fn start_upstream(role: &'static str, proxy: StatusCode) -> (String, Upstream0) {
        let up = Upstream0 {
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

    async fn ha_status(State(up): State<Upstream0>) -> Json<serde_json::Value> {
        let role = *up.role.lock().expect("role lock");
        Json(serde_json::json!({ "role": role }))
    }

    async fn proxied(State(up): State<Upstream0>) -> Response {
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

    fn ha_edge(urls: &[&str]) -> Arc<Edge> {
        ha_edge_with_timeout(urls, Duration::from_secs(10))
    }

    fn ha_edge_with_timeout(urls: &[&str], timeout: Duration) -> Arc<Edge> {
        Edge::new(EdgeConfig {
            upstream: Upstream::Ha(HaUpstream {
                upstreams: urls.iter().map(|u| (*u).to_string()).collect(),
                token: "token".into(),
            }),
            upstream_timeout: timeout,
            allowed_sources: vec![],
        })
        .expect("edge")
    }

    /// A relay whose upstream is a plain function, the way the node daemon
    /// drives it.
    fn fixed_edge(url: &str) -> Arc<Edge> {
        let url = url.to_string();
        Edge::new(EdgeConfig {
            upstream: Upstream::Fixed(Arc::new(move || url.clone())),
            upstream_timeout: Duration::from_secs(10),
            allowed_sources: vec![],
        })
        .expect("relay")
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
        let e = ha_edge(&[&standby, &leader]);
        assert_eq!(e.leader().await.as_deref(), Some(leader.as_str()));
    }

    #[tokio::test]
    async fn unreachable_upstreams_are_leaderless() {
        let e = ha_edge(&["http://127.0.0.1:1"]);
        assert!(e.leader().await.is_none());
    }

    #[tokio::test]
    async fn leaderless_cluster_answers_503_with_retry_after() {
        let (standby, _a) = start_upstream("standby", StatusCode::OK).await;
        let e = ha_edge(&[&standby]);
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
        let e = ha_edge(&[&old, &new]);

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
        let e = ha_edge(&[&a, &b]);

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
        let e = ha_edge_with_timeout(&[&silent, &fresh], Duration::from_millis(300));

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
        let e = ha_edge_with_timeout(&[&a, &b], Duration::from_millis(200));

        let resp = proxy(State(e.clone()), get_request("/v1/tasks")).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(e.metrics.upstream_timeouts_total.load(Ordering::Relaxed), 2);
        assert_eq!(e.metrics.upstream_errors_total.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn proxied_response_keeps_upstream_headers() {
        let (leader, _a) = start_upstream("leader", StatusCode::OK).await;
        let e = ha_edge(&[&leader]);
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
        let e = ha_edge(&["http://127.0.0.1:1"]);
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
        assert!(text.contains("agentgrid_edge_rejected_sources_total 0"));
    }

    #[tokio::test]
    async fn fixed_upstream_forwards_without_discovery() {
        // The node-hosted relay: no /v1/ha/status call, no cluster token. The
        // upstream is whatever the daemon's `base()` says right now.
        let (cp, up) = start_upstream("standby", StatusCode::OK).await;
        let e = fixed_edge(&cp);
        let resp = proxy(State(e.clone()), get_request("/v1/tasks")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(up.hits.load(Ordering::Relaxed), 1);
        assert_eq!(
            e.metrics.leader_discoveries_total.load(Ordering::Relaxed),
            0,
            "a fixed upstream must not ask for a leader"
        );
    }

    #[tokio::test]
    async fn fixed_upstream_is_re_read_every_request() {
        // This is what makes the relay follow a leader move: the daemon updates
        // its endpoint, and the very next forwarded request goes to the new
        // one — no TTL to wait out.
        let (first, first_up) = start_upstream("leader", StatusCode::OK).await;
        let (second, second_up) = start_upstream("leader", StatusCode::OK).await;
        let current = Arc::new(Mutex::new(first));
        let slot = Arc::clone(&current);
        let e = Edge::new(EdgeConfig {
            upstream: Upstream::Fixed(Arc::new(move || slot.lock().expect("slot").clone())),
            upstream_timeout: Duration::from_secs(10),
            allowed_sources: vec![],
        })
        .expect("relay");

        let _ = proxy(State(e.clone()), get_request("/v1/tasks")).await;
        *current.lock().expect("slot") = second.clone();
        let _ = proxy(State(e.clone()), get_request("/v1/tasks")).await;

        assert_eq!(first_up.hits.load(Ordering::Relaxed), 1);
        assert_eq!(second_up.hits.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn empty_fixed_upstream_is_leaderless_not_a_panic() {
        let e = fixed_edge("");
        assert!(e.leader().await.is_none());
        let resp = proxy(State(e), get_request("/v1/tasks")).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn cidr_accepts_prefixes_addresses_and_rejects_junk() {
        assert!(Cidr::parse("10.0.0.0/8").is_ok());
        assert!(Cidr::parse("10.0.0.5").is_ok());
        assert!(Cidr::parse("::1").is_ok());
        assert!(Cidr::parse("2001:db8::/32").is_ok());
        assert!(Cidr::parse("10.0.0.0/33").is_err(), "prefix too long");
        assert!(Cidr::parse("not-an-ip").is_err());
        assert!(Cidr::parse("10.0.0.0/").is_err());
    }

    #[test]
    fn cidr_contains_only_its_own_family_and_range() {
        let lan = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(lan.contains("10.1.2.3".parse().unwrap()));
        assert!(!lan.contains("11.0.0.1".parse().unwrap()));
        // A v4 entry must not match a v6 peer, even a mapped one.
        assert!(!lan.contains("::ffff:10.1.2.3".parse().unwrap()));

        let one = Cidr::parse("192.168.1.5").unwrap();
        assert!(one.contains("192.168.1.5".parse().unwrap()));
        assert!(!one.contains("192.168.1.6".parse().unwrap()));

        let v6 = Cidr::parse("2001:db8::/32").unwrap();
        assert!(v6.contains("2001:db8::1".parse().unwrap()));
        assert!(!v6.contains("2001:db9::1".parse().unwrap()));

        let all = Cidr::parse("0.0.0.0/0").unwrap();
        assert!(all.contains(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!all.contains(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn source_gate_opens_up_when_the_list_is_empty() {
        let open = fixed_edge("http://127.0.0.1:1");
        assert!(open.source_allowed("203.0.113.9".parse().unwrap()));

        let closed = Edge::new(EdgeConfig {
            upstream: Upstream::Fixed(Arc::new(|| "http://127.0.0.1:1".into())),
            upstream_timeout: Duration::from_secs(10),
            allowed_sources: vec![Cidr::parse("10.0.0.0/8").unwrap()],
        })
        .expect("relay");
        assert!(closed.source_allowed("10.9.9.9".parse().unwrap()));
        assert!(!closed.source_allowed("203.0.113.9".parse().unwrap()));
    }

    #[test]
    fn allowlist_spec_parses_lists_and_skips_blanks() {
        assert!(parse_allowlist(None).unwrap().is_empty());
        assert!(parse_allowlist(Some("")).unwrap().is_empty());
        assert!(parse_allowlist(Some(" , ")).unwrap().is_empty());
        let list = parse_allowlist(Some("10.0.0.0/8, 192.168.1.5 ,")).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[1].contains("192.168.1.5".parse().unwrap()));
        // A typo in one entry must fail the whole list rather than silently
        // widening or narrowing who may use the relay.
        assert!(parse_allowlist(Some("10.0.0.0/8,10.0.0.0/99")).is_err());
    }
}
