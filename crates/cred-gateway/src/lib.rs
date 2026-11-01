//! Credential gateway (ADR 0029, option F/inject).
//!
//! The sandbox never holds a provider key. It holds a **capability**: an opaque
//! string minted for one attempt, with a TTL and an optional request quota. The
//! gateway validates it, swaps it for the real credential on its own outbound
//! leg, and streams the answer back.
//!
//! Why this shape rather than a broker (ADR 0029, option E): the adapter is a
//! wrapper, so the process that talks to the provider is a third-party CLI we
//! do not control. The only thing we can hand it is an environment variable —
//! so the credential has to be replaced *in the network path*, not in the
//! adapter. That is this process.
//!
//! Three properties are load-bearing and each has a test:
//!
//! - the real key is only ever written into the outbound request, never into a
//!   response body, an error message or a log line;
//! - redirects are **not** followed. A `302` to another host would replay the
//!   injected header to a destination the operator never allowed;
//! - a capability is bound to one route, so a token minted for `anthropic`
//!   cannot be replayed against `openai`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

/// Request bodies are buffered before forwarding: the upstream is an LLM API,
/// so the payload is a prompt (large, but not unbounded), and buffering keeps
/// the request path free of back-pressure subtleties. Responses stream.
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

/// Headers that must not be forwarded in either direction.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

#[derive(Clone, Debug)]
pub struct Route {
    /// Upstream base, no trailing slash, e.g. `https://api.anthropic.com`.
    pub upstream: String,
    /// Header the provider expects the credential in, lowercase:
    /// `x-api-key` for Anthropic, `authorization` for an OpenAI-shaped API.
    pub header: String,
}

#[derive(Debug)]
struct Cap {
    attempt_id: String,
    route: String,
    expires: Instant,
    quota: Option<u32>,
    used: u32,
}

pub struct Gateway {
    routes: HashMap<String, Route>,
    keys: HashMap<String, String>,
    admin: String,
    caps: Mutex<HashMap<String, Cap>>,
    http: reqwest::Client,
}

/// Hand-written so that a stray `{:?}` — in a panic message, in a log line —
/// can never print a provider key or the admin token.
impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("routes", &self.routes.keys().collect::<Vec<_>>())
            .field("keys", &format_args!("<{} redacted>", self.keys.len()))
            .field("admin", &"<redacted>")
            .field(
                "caps",
                &self.caps.lock().map(|c| c.len()).unwrap_or_default(),
            )
            .finish()
    }
}

impl Gateway {
    /// Fails closed: a route without a key, or an empty admin token, is a
    /// configuration mistake that must not start a proxy that would either
    /// forward an empty credential or accept anonymous minting.
    pub fn new(
        routes: HashMap<String, Route>,
        keys: HashMap<String, String>,
        admin: String,
    ) -> anyhow::Result<Self> {
        if admin.trim().is_empty() {
            anyhow::bail!("AGENTGRID_CRED_GATEWAY_ADMIN is empty: the mint API would be open");
        }
        if routes.is_empty() {
            anyhow::bail!("no routes configured (AGENTGRID_CRED_GATEWAY_ROUTES)");
        }
        for name in routes.keys() {
            if !keys.contains_key(name) {
                anyhow::bail!("route '{name}' has no key (AGENTGRID_CRED_GATEWAY_KEYS)");
            }
        }
        let http = reqwest::Client::builder()
            // Not following redirects is a security property, not a preference:
            // reqwest re-sends the request (including the header we just
            // injected) to the redirect target.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            routes,
            keys,
            admin,
            caps: Mutex::new(HashMap::new()),
            http,
        })
    }

    pub fn from_env() -> anyhow::Result<Self> {
        let routes = parse_routes(&env("AGENTGRID_CRED_GATEWAY_ROUTES").unwrap_or_default())?;
        let keys = parse_keys(&env("AGENTGRID_CRED_GATEWAY_KEYS").unwrap_or_default());
        let admin = env("AGENTGRID_CRED_GATEWAY_ADMIN").unwrap_or_default();
        Self::new(routes, keys, admin)
    }
}

pub fn router(gw: Arc<Gateway>) -> Router {
    Router::new()
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({"ok": true})) }),
        )
        .route("/tokens", post(mint))
        .route("/tokens/{token}", delete(revoke))
        .route("/p/{route}/{*rest}", any(proxy))
        .with_state(gw)
}

#[derive(Deserialize)]
struct MintRequest {
    attempt_id: String,
    route: String,
    ttl_secs: u64,
    /// Maximum number of proxied requests for this attempt. `None` = no limit.
    quota: Option<u32>,
}

#[derive(Serialize)]
struct MintResponse {
    token: String,
    route: String,
    expires_in: u64,
}

async fn mint(
    State(gw): State<Arc<Gateway>>,
    headers: HeaderMap,
    Json(req): Json<MintRequest>,
) -> Response {
    if !admin_ok(&gw, &headers) {
        return problem(StatusCode::UNAUTHORIZED, "admin token required");
    }
    if !gw.routes.contains_key(&req.route) {
        return problem(StatusCode::NOT_FOUND, "unknown route");
    }
    let ttl = req.ttl_secs.clamp(1, 24 * 60 * 60);
    let token = new_token();
    {
        let mut caps = gw.caps.lock().expect("caps lock");
        let now = Instant::now();
        caps.retain(|_, c| c.expires > now);
        caps.insert(
            token.clone(),
            Cap {
                attempt_id: req.attempt_id,
                route: req.route.clone(),
                expires: now + Duration::from_secs(ttl),
                quota: req.quota,
                used: 0,
            },
        );
    }
    tracing::info!(route = %req.route, ttl, "capability minted");
    (
        StatusCode::OK,
        Json(MintResponse {
            token,
            route: req.route,
            expires_in: ttl,
        }),
    )
        .into_response()
}

async fn revoke(
    State(gw): State<Arc<Gateway>>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response {
    if !admin_ok(&gw, &headers) {
        return problem(StatusCode::UNAUTHORIZED, "admin token required");
    }
    let removed = gw.caps.lock().expect("caps lock").remove(&token).is_some();
    if removed {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn proxy(
    State(gw): State<Arc<Gateway>>,
    Path((route_name, rest)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    req: Request,
) -> Response {
    let Some(route) = gw.routes.get(&route_name) else {
        return problem(StatusCode::NOT_FOUND, "unknown route");
    };
    let headers = req.headers().clone();
    let Some(capability) = capability_from(&headers, &route.header) else {
        return problem(StatusCode::UNAUTHORIZED, "capability required");
    };

    // Validate and account under one lock: two concurrent requests must not
    // both pass a quota of one.
    let attempt_id = {
        let mut caps = gw.caps.lock().expect("caps lock");
        let now = Instant::now();
        let Some(cap) = caps.get_mut(&capability) else {
            return problem(StatusCode::UNAUTHORIZED, "unknown capability");
        };
        if cap.expires <= now {
            caps.remove(&capability);
            return problem(StatusCode::UNAUTHORIZED, "capability expired");
        }
        if cap.route != route_name {
            return problem(StatusCode::FORBIDDEN, "capability is for another route");
        }
        if let Some(quota) = cap.quota {
            if cap.used >= quota {
                return problem(StatusCode::TOO_MANY_REQUESTS, "attempt quota exhausted");
            }
        }
        cap.used += 1;
        cap.attempt_id.clone()
    };

    let Some(secret) = gw.keys.get(&route_name) else {
        // Unreachable: `new` refuses a route without a key. Kept as a hard
        // failure rather than a fallback that would forward an empty credential.
        return problem(StatusCode::INTERNAL_SERVER_ERROR, "route has no key");
    };

    let method = match reqwest::Method::from_bytes(req.method().as_str().as_bytes()) {
        Ok(m) => m,
        Err(_) => return problem(StatusCode::METHOD_NOT_ALLOWED, "bad method"),
    };
    let mut url = format!("{}/{}", route.upstream.trim_end_matches('/'), rest);
    if let Some(q) = query.as_deref().filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(q);
    }

    let body = match axum::body::to_bytes(req.into_body(), MAX_REQUEST_BYTES).await {
        Ok(b) => b,
        Err(_) => return problem(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };

    let mut out = gw.http.request(method, &url);
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) || lower == route.header {
            continue;
        }
        out = out.header(name.as_str(), value.as_bytes());
    }
    out = out.header(
        route.header.as_str(),
        credential_value(&route.header, secret),
    );
    if !body.is_empty() {
        out = out.body(body);
    }

    let upstream = match out.send().await {
        Ok(r) => r,
        Err(e) => {
            // The error text can contain the URL but never the credential: the
            // header is not part of `reqwest::Error`'s Display.
            tracing::warn!(route = %route_name, attempt = %attempt_id, error = %e, "upstream request failed");
            return problem(StatusCode::BAD_GATEWAY, "upstream request failed");
        }
    };

    let status = upstream.status();
    let mut response = Response::builder().status(status);
    for (name, value) in upstream.headers().iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) {
            continue;
        }
        response = response.header(name.as_str(), value.as_bytes());
    }
    let body = Body::from_stream(upstream.bytes_stream());
    response
        .body(body)
        .unwrap_or_else(|_| problem(StatusCode::BAD_GATEWAY, "could not build the response"))
}

/// The capability arrives in the same header the provider expects, because the
/// CLI is configured with it: `ANTHROPIC_API_KEY=<capability>` makes `claude`
/// send `x-api-key: <capability>`. `Authorization: Bearer` is accepted as a
/// fallback for clients that only speak that one.
fn capability_from(headers: &HeaderMap, route_header: &str) -> Option<String> {
    let direct = headers
        .get(route_header)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    if route_header == "authorization" {
        bearer.or(direct)
    } else {
        direct.or(bearer)
    }
}

fn credential_value(header: &str, secret: &str) -> String {
    if header == "authorization" {
        format!("Bearer {secret}")
    } else {
        secret.to_string()
    }
}

fn admin_ok(gw: &Gateway, headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| constant_time_eq(t.as_bytes(), gw.admin.as_bytes()))
        .unwrap_or(false)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn new_token() -> String {
    // 256 bits from two v4 UUIDs: no extra dependency, and the OS RNG behind
    // `uuid::new_v4` is the same one a dedicated crate would use.
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn problem(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `name|header|https://upstream,other|authorization|https://…`
pub fn parse_routes(spec: &str) -> anyhow::Result<HashMap<String, Route>> {
    let mut out = HashMap::new();
    for entry in spec.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let parts: Vec<&str> = entry.split('|').map(|s| s.trim()).collect();
        if parts.len() != 3 {
            anyhow::bail!(
                "route '{entry}' is not 'name|header|https://upstream' (3 fields, got {})",
                parts.len()
            );
        }
        let (name, header, upstream) = (parts[0], parts[1].to_ascii_lowercase(), parts[2]);
        if name.is_empty() || upstream.is_empty() {
            anyhow::bail!("route '{entry}' has an empty name or upstream");
        }
        if !(upstream.starts_with("https://") || upstream.starts_with("http://")) {
            anyhow::bail!("route '{name}': upstream must be an http(s) URL, got '{upstream}'");
        }
        out.insert(
            name.to_string(),
            Route {
                upstream: upstream.trim_end_matches('/').to_string(),
                header,
            },
        );
    }
    Ok(out)
}

/// `name=secret,other=secret`. The secret may contain `=`; only the first one
/// separates the name.
pub fn parse_keys(spec: &str) -> HashMap<String, String> {
    spec.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_parse_and_normalise() {
        let r = parse_routes(
            "anthropic|x-api-key|https://api.anthropic.com/,openai|Authorization|http://gw",
        )
        .unwrap();
        assert_eq!(r["anthropic"].upstream, "https://api.anthropic.com");
        assert_eq!(r["anthropic"].header, "x-api-key");
        assert_eq!(r["openai"].header, "authorization");
    }

    #[test]
    fn a_route_with_two_fields_is_refused() {
        let err = parse_routes("anthropic|https://api.anthropic.com").unwrap_err();
        assert!(err.to_string().contains("3 fields"), "{err}");
    }

    #[test]
    fn an_upstream_that_is_not_a_url_is_refused() {
        let err = parse_routes("x|x-api-key|api.anthropic.com").unwrap_err();
        assert!(err.to_string().contains("http(s)"), "{err}");
    }

    #[test]
    fn a_key_containing_equals_survives() {
        let k = parse_keys("anthropic=sk-ant=abc=,openai=sk-1");
        assert_eq!(k["anthropic"], "sk-ant=abc=");
        assert_eq!(k["openai"], "sk-1");
    }

    #[test]
    fn a_route_without_a_key_refuses_to_start() {
        let routes = parse_routes("anthropic|x-api-key|https://api.anthropic.com").unwrap();
        let err = Gateway::new(routes, HashMap::new(), "admin".into()).unwrap_err();
        assert!(err.to_string().contains("no key"), "{err}");
    }

    #[test]
    fn an_empty_admin_token_refuses_to_start() {
        let routes = parse_routes("a|authorization|https://x").unwrap();
        let mut keys = HashMap::new();
        keys.insert("a".to_string(), "k".to_string());
        let err = Gateway::new(routes, keys, "  ".into()).unwrap_err();
        assert!(err.to_string().contains("ADMIN"), "{err}");
    }

    #[test]
    fn the_capability_is_read_from_the_route_header_first() {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", "cap-from-route".parse().unwrap());
        h.insert("authorization", "Bearer cap-from-bearer".parse().unwrap());
        assert_eq!(
            capability_from(&h, "x-api-key").as_deref(),
            Some("cap-from-route")
        );
        assert_eq!(
            capability_from(&h, "authorization").as_deref(),
            Some("cap-from-bearer")
        );
    }

    #[test]
    fn a_bearer_prefixed_value_is_accepted_for_a_route_header() {
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer abc".parse().unwrap());
        assert_eq!(capability_from(&h, "x-api-key").as_deref(), Some("abc"));
    }

    #[test]
    fn the_injected_value_keeps_the_bearer_prefix_only_where_it_belongs() {
        assert_eq!(credential_value("x-api-key", "sk-1"), "sk-1");
        assert_eq!(credential_value("authorization", "sk-1"), "Bearer sk-1");
    }

    #[test]
    fn constant_time_eq_is_still_an_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
