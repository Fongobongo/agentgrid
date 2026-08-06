//! Authentication: user JWT sessions, node credentials, and the auth routes.

use std::sync::Arc;

use agentgrid_common::{LoginRequest, LoginResponse, SetupRequest};
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Request, StatusCode},
    middleware::Next,
    response::Response,
    Json,
};
use jsonwebtoken::{decode, DecodingKey, Validation};
use serde::{Deserialize, Serialize};

use crate::AppState;

/// JWT claims for user sessions (Stage 4.1).
/// Includes `jti` (JWT ID) for session revocation (Stage 4.2).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Claims {
    pub(crate) sub: String,
    pub(crate) exp: usize,
    pub(crate) jti: String,
}

/// Stage 2.5: the cookie name carrying the session JWT, set HttpOnly so the
/// browser cannot read it (no XSS token theft) with SameSite=Strict (CSRF
/// guard). `Secure` is added only when `AGENTGRID_COOKIE_SECURE=1` so local
/// plaintext dev keeps working.
const AUTH_COOKIE: &str = "agentgrid_token";

/// Extract a session JWT from a request: an `Authorization: Bearer` header
/// (non-browser clients: CLI, gateway, node) or the `agentgrid_token` cookie
/// (browser fetch with `credentials: include`).
pub fn auth_token_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(h) = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
    {
        return Some(h.to_string());
    }
    headers
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .and_then(|c| {
            c.split(';')
                .map(|p| p.trim())
                .find_map(|p| p.strip_prefix(&format!("{AUTH_COOKIE}=")))
        })
        .map(|s| s.to_string())
}

/// Build a `Set-Cookie` header value for a freshly-issued session JWT.
pub fn auth_cookie_header(token: &str) -> String {
    let secure = std::env::var("AGENTGRID_COOKIE_SECURE").as_deref() == Ok("1");
    let mut v = format!("{AUTH_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200");
    if secure {
        v.push_str("; Secure");
    }
    v
}

/// User identity established by [`require_user_auth`]; read by user handlers.
#[derive(Clone)]
pub struct AuthedUser {
    pub username: String,
}

#[derive(Clone)]
pub(crate) struct AuthedNode {
    pub(crate) node_id: String,
}

/// Enforce Bearer node-credential auth on all `/v1/node/` routes except enroll.
pub async fn require_node_auth(
    State(state): State<Arc<AppState>>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = req.uri().path().to_string();
    if path.starts_with("/v1/node/") && path != "/v1/node/enroll" {
        let cred = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "));
        match cred {
            Some(c) => match state.store.node_id_for_credential(c).await {
                Ok(Some(node_id)) => {
                    req.extensions_mut().insert(AuthedNode { node_id });
                    Ok(next.run(req).await)
                }
                _ => Err(StatusCode::UNAUTHORIZED),
            },
            None => Err(StatusCode::UNAUTHORIZED),
        }
    } else {
        Ok(next.run(req).await)
    }
}

/// Reject a node request whose attempt is not owned by the authenticated
/// node. Hardening P0: cross-node isolation. Returns `Ok(())` if the attempt
/// exists and belongs to `auth.node_id`; `Err(FORBIDDEN)` if it exists but
/// belongs to another node; `Err(NOT_FOUND)` if no such attempt (avoid
/// disclosing existence). Optionally allow a revoked caller to also hit 403 —
/// but revoked nodes already fail at `require_node_auth` (no credential match).
pub async fn check_attempt_owner(
    state: &AppState,
    auth: &AuthedNode,
    attempt_id: &str,
) -> Result<(), StatusCode> {
    match state.store.attempt_owner(attempt_id).await {
        Ok(Some(node_id)) if node_id == auth.node_id => Ok(()),
        Ok(Some(_)) => {
            state
                .cross_node_rejects
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(StatusCode::FORBIDDEN)
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("attempt_owner failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Hardening P0 item 8: verify the fencing token presented by this node still
/// matches the current attempt's stored token. Returns Ok(()) if it matches
/// (or the attempt has no token, N-1 backcompat); Err(StatusCode) otherwise:
/// `409 Conflict` for a stale token (the node is reporting for an attempt
/// that was reassigned or lost), `404/500` for missing/DB error.
pub async fn check_fencing_token(
    state: &AppState,
    attempt_id: &str,
    presented: Option<&str>,
) -> Result<(), StatusCode> {
    let stored = sqlx::query_scalar::<_, String>("SELECT fencing_token FROM attempts WHERE id = ?")
        .bind(attempt_id)
        .fetch_optional(&state.store.pool)
        .await;
    match stored {
        Ok(None) => Err(StatusCode::NOT_FOUND),
        // N-1 backcompat: a legacy attempt (or legacy node on a freshly
        // migrated CP) has a blank token; accept only a legacy presenter
        // (no header) to avoid letting any arbitrary token hijack it.
        Ok(Some(s)) if s.is_empty() => {
            if presented.is_none_or(|p| p.is_empty()) {
                Ok(())
            } else {
                state
                    .stale_fencing_tokens
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Err(StatusCode::CONFLICT)
            }
        }
        Ok(Some(s)) if Some(s.as_str()) == presented => Ok(()),
        Ok(Some(_)) => {
            state
                .stale_fencing_tokens
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(StatusCode::CONFLICT)
        }
        Err(e) => {
            tracing::error!("fencing_token check failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Extract the node-presented fencing token from the request headers
/// (`X-AgentGrid-Fencing-Token`). None when absent (N-1 nodes).
pub fn fencing_token_header(h: &HeaderMap) -> Option<String> {
    h.get("x-agentgrid-fencing-token")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

pub async fn health_live() -> StatusCode {
    StatusCode::OK
}

/// Whether a path requires a user JWT (Stage 4.1). Node auth (`/v1/node/*`)
/// and the auth endpoints themselves are exempt; health/metrics are public.
pub fn user_protected(path: &str) -> bool {
    if path.starts_with("/health") || path == "/metrics" {
        return false;
    }
    if path.starts_with("/v1/node/") {
        return false;
    }
    if path == "/v1/auth/login" || path == "/v1/auth/setup" || path == "/v1/auth/logout" {
        return false;
    }
    true
}

/// Require a valid user JWT on user-facing routes. Hardening P0:
/// - DB error in `user_count` fails closed (503), never opens the API.
/// - Before the first user exists (bootstrap not complete), all `/v1/` user
///   routes are closed (503) except `/v1/auth/setup`; static UI (non-`/v1/`
///   paths) stays served so the setup page loads. Node routes are handled
///   by [`require_node_auth`] and are skipped here.
pub async fn require_user_auth(
    State(state): State<Arc<AppState>>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = req.uri().path().to_string();
    if user_protected(&path) {
        match state.store.user_count().await {
            Ok(0) => {
                // Bootstrap not complete: only setup (exempt above) and static
                // UI (non-/v1/ paths) are served; everything else is closed.
                if path.starts_with("/v1/") {
                    return Err(StatusCode::SERVICE_UNAVAILABLE);
                }
            }
            Ok(_) => {
                if let Some(token) = auth_token_from_headers(req.headers()) {
                    match state.verify_token(&token).await {
                        Some(u) => {
                            req.extensions_mut().insert(AuthedUser { username: u });
                        }
                        None => return Err(StatusCode::UNAUTHORIZED),
                    }
                } else {
                    return Err(StatusCode::UNAUTHORIZED);
                }
            }
            // Fail closed: a DB outage never opens the API.
            Err(e) => {
                tracing::error!("user_count failed in auth middleware: {e}");
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
        }
    }
    Ok(next.run(req).await)
}

pub async fn auth_setup(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SetupRequest>,
) -> Result<(StatusCode, HeaderMap, Json<LoginResponse>), StatusCode> {
    if req.username.is_empty() || req.password.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Only allowed while no users exist (closes the open bootstrap window).
    match state.store.user_count().await {
        Ok(0) => {}
        Ok(_) => return Err(StatusCode::CONFLICT),
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
    // Hardening P0: require the one-time setup token minted at first start.
    // Rejects missing/expired/already-consumed tokens; the comparison is
    // constant-time-ish via a simple byte eq (token is high-entropy and
    // short-lived, so timing leakage is not a practical concern).
    {
        let mut guard = state.setup_token.lock().await;
        let valid = match guard.as_ref() {
            Some(t) if t.is_live() => t.token == req.setup_token.as_deref().unwrap_or(""),
            _ => false,
        };
        if !valid {
            return Err(StatusCode::FORBIDDEN);
        }
        // Consume: the token is single-use.
        *guard = None;
    }
    match state.store.create_user(&req.username, &req.password).await {
        Ok(true) => {}
        Ok(false) => return Err(StatusCode::CONFLICT),
        Err(e) => {
            tracing::error!("create_user failed: {e}");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    let token = state.issue_token(&req.username).map_err(|e| {
        tracing::error!("issue_token failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let _ = state
        .store
        .audit("user", Some(&req.username), "user.create", None, None)
        .await;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        header::HeaderValue::from_str(&auth_cookie_header(&token))
            .unwrap_or_else(|_| header::HeaderValue::from_static("")),
    );
    Ok((StatusCode::CREATED, headers, Json(LoginResponse { token })))
}

pub async fn auth_login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<(HeaderMap, Json<LoginResponse>), StatusCode> {
    // Stage 2.5: brute-force protection. Fail closed to 429 on budget
    // exhaustion; the generic error avoids user enumeration.
    {
        let mut rate = state.login_rate.lock().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if !rate.check_and_record(&req.username.to_lowercase(), now) {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
    }
    let user = state
        .store
        .verify_user(&req.username, &req.password)
        .await
        .map_err(|e| {
            tracing::error!("verify_user failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let Some(_) = user else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let token = state.issue_token(&req.username).map_err(|e| {
        tracing::error!("issue_token failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let _ = state
        .store
        .audit("user", Some(&req.username), "login", None, None)
        .await;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        header::HeaderValue::from_str(&auth_cookie_header(&token))
            .unwrap_or_else(|_| header::HeaderValue::from_static("")),
    );
    Ok((headers, Json(LoginResponse { token })))
}

pub async fn auth_logout(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> (HeaderMap, StatusCode) {
    // Clear the session cookie and revoke the JWT jti (Stage 4.2).
    if let Some(token) = auth_token_from_headers(req.headers()) {
        if let Ok(claims) = decode::<Claims>(
            &token,
            &DecodingKey::from_secret(&state.jwt_secret),
            &Validation::default(),
        ) {
            let _ = state
                .store
                .revoke_session(&claims.claims.jti, &claims.claims.sub)
                .await;
        }
    }
    // Clear the session cookie regardless of auth state (idempotent logout).
    let mut v = format!("{AUTH_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    if std::env::var("AGENTGRID_COOKIE_SECURE").as_deref() == Ok("1") {
        v.push_str("; Secure");
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        header::HeaderValue::from_str(&v).unwrap_or_else(|_| header::HeaderValue::from_static("")),
    );
    (headers, StatusCode::OK)
}

pub async fn health_ready(State(state): State<Arc<AppState>>) -> StatusCode {
    let dir = std::path::Path::new(&state.db_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    let probe = dir.join(".agentgrid-health-probe");
    let writable = tokio::task::spawn_blocking(move || {
        let ok = std::fs::write(&probe, b"ok").is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    })
    .await
    .unwrap_or(false);
    if state.store.health_check().await && writable {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}
