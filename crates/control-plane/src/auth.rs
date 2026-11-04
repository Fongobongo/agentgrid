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
/// Includes `jti` (JWT ID) for session revocation (Stage 4.2) and `role`
/// for RBAC (plan 5.2).
///
/// Finding 7: `role` is **not** authoritative — [`AppState::verify_token`]
/// reads the role from the `users` table on every request, so a demoted admin
/// loses the role immediately. The claim stays for diagnostics and because
/// older tokens carry it, but it is never trusted. A token without a `role`
/// claim is not "admin" any more; it simply has no claim, and the table
/// decides.
///
/// `gen` is the session generation stamped at login. A change to the role or
/// the password increments the row's counter, and every token carrying an
/// older value is refused — which is how revocation reaches sessions that are
/// already in the wild. A token minted before this field existed is generation
/// 0, and is accepted only while the account is still at generation 0.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Claims {
    pub(crate) sub: String,
    pub(crate) exp: usize,
    #[serde(default)]
    pub(crate) iat: usize,
    #[serde(default)]
    pub(crate) gen: i64,
    pub(crate) jti: String,
    #[serde(default)]
    pub(crate) role: String,
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

/// Minimum password length for a user account (finding 7). The API used to
/// accept anything non-empty, so `{"password":"a"}` created an account.
pub const MIN_PASSWORD_LEN: usize = 12;

/// True when a password meets the policy. Counted in characters, not bytes, so
/// a Cyrillic or CJK passphrase is not penalised.
pub fn password_is_acceptable(password: &str) -> bool {
    password.chars().count() >= MIN_PASSWORD_LEN
}

/// Whether the session cookie carries `Secure`.
///
/// `AGENTGRID_COOKIE_SECURE=1` forces it on. It is also on automatically when
/// the control plane serves TLS itself or runs with `AGENTGRID_ENV=production`
/// — in production the browser reaches the control plane over a TLS terminator
/// in front of it, so a non-`Secure` cookie is the anomaly (finding 16). The
/// env var stays as an escape hatch for a plaintext dev host.
fn cookie_secure() -> bool {
    if std::env::var("AGENTGRID_COOKIE_SECURE").as_deref() == Ok("1") {
        return true;
    }
    let tls =
        std::env::var("AGENTGRID_TLS_CERT").is_ok() && std::env::var("AGENTGRID_TLS_KEY").is_ok();
    // Boot validates AGENTGRID_ENV and refuses unknown values; the fail-closed
    // accessor means a skipped validation can only add `Secure`, never drop it.
    tls || agentgrid_common::deploy_env_fail_closed().is_production()
}

/// Build a `Set-Cookie` header value for a freshly-issued session JWT.
pub fn auth_cookie_header(token: &str) -> String {
    let mut v = format!("{AUTH_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200");
    if cookie_secure() {
        v.push_str("; Secure");
    }
    v
}

/// User identity established by [`require_user_auth`]; read by user handlers.
#[derive(Clone)]
pub struct AuthedUser {
    pub username: String,
    /// RBAC role (plan 5.2): "admin" or "operator".
    pub role: String,
}

/// RBAC (plan 5.2, finding 8): an operator may view (GET/HEAD) and
/// approve/deny, but nothing else mutates. All other mutating verbs are
/// rejected with 403 by [`require_user_auth`] before the handler runs.
///
/// Read access is an **allow-list**, not "any GET". The old rule was
/// "every GET except `/v1/adapter-env`", which fails open: every new GET
/// route that returns sensitive data was readable by an operator the moment it
/// was registered. A route has to be declared here to be readable, so the
/// default for a new GET is admin-only.
const OPERATOR_READ: &[&str] = &[
    "/v1/agents",
    "/v1/agents/{id}/actions",
    "/v1/approvals",
    "/v1/approvals/{id}",
    "/v1/attempts/{id}",
    "/v1/attempts/{id}/annotations",
    "/v1/audit",
    "/v1/cluster/status",
    "/v1/conversations",
    "/v1/conversations/{id}",
    "/v1/conversations/{id}/messages",
    "/v1/mcp-servers",
    "/v1/node-installs",
    "/v1/node-installs/{id}",
    "/v1/nodes",
    "/v1/nodes/{id}",
    "/v1/nodes/{id}/accounts/usage",
    "/v1/nodes/{id}/opencode-audit",
    "/v1/opencode-profiles",
    "/v1/opencode-profiles/{name}",
    "/v1/profiles",
    "/v1/profiles/{id}",
    "/v1/provisioning",
    "/v1/repos/{repo}/learnings",
    "/v1/repositories",
    "/v1/search",
    "/v1/search/events",
    "/v1/skills",
    "/v1/skills/{name}",
    "/v1/stream",
    "/v1/task-groups/{id}/context",
    "/v1/task-groups/{id}/context/{key}",
    "/v1/tasks",
    "/v1/tasks/{id}",
    "/v1/tasks/{id}/artifacts",
    "/v1/tasks/{id}/artifacts/{name}",
    "/v1/tasks/{id}/eligibility",
    "/v1/tasks/{id}/events",
    "/v1/tasks/{id}/events/stream",
    "/v1/tasks/{id}/review-approval",
    "/v1/tasks/{id}/tags",
    "/v1/users",
    "/v1/workflow-runs",
    "/v1/workflow-runs/{id}",
    "/v1/workflow-runs/{id}/plan",
    "/v1/workflow-runs/{id}/projection",
    "/v1/workflows",
    "/v1/workflows/{id}",
    "/v1/workflows/{id}/schedules",
];

/// GET routes an operator may not read, kept explicit so the classification
/// test below can tell "deliberately closed" from "forgotten".
const OPERATOR_READ_DENIED: &[&str] = &[
    // Secrets are not "view" data. The list carries fingerprints rather than
    // values (ADR 0028 Phase 0), but even the fingerprint is an oracle — it
    // confirms a guessed key — and enumerating which credentials exist on
    // which node is itself information.
    "/v1/adapter-env",
    // Proxy URLs routinely carry `user:pass@`; the panel only needs to
    // recognise which proxy is configured.
    "/v1/proxies",
];

/// Match a concrete request path against a route template, where a `{name}`
/// segment matches exactly one path segment.
fn path_matches(template: &str, path: &str) -> bool {
    let mut t = template.split('/');
    let mut p = path.split('/');
    loop {
        match (t.next(), p.next()) {
            (None, None) => return true,
            (Some(t), Some(p)) => {
                let is_param = t.starts_with('{') && t.ends_with('}');
                if is_param {
                    if p.is_empty() {
                        return false;
                    }
                } else if t != p {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

fn operator_allowed(method: &str, path: &str) -> bool {
    if method == "GET" || method == "HEAD" {
        // Deny wins even if a later edit adds the path to the read list: these
        // two are the ones whose payload is a credential.
        if OPERATOR_READ_DENIED.iter().any(|t| path_matches(t, path)) {
            return false;
        }
        return OPERATOR_READ.iter().any(|t| path_matches(t, path));
    }
    if method == "POST" {
        if path == "/v1/auth/logout" {
            return true;
        }
        if path.starts_with("/v1/approvals/")
            && (path.ends_with("/allow") || path.ends_with("/deny"))
        {
            return true;
        }
        if path.starts_with("/v1/workflow-runs/") && path.ends_with("/approve-plan") {
            return true;
        }
    }
    false
}

#[derive(Clone)]
pub(crate) struct AuthedNode {
    pub(crate) node_id: String,
}

/// Node-facing routes that must work before the caller has a credential.
///
/// `/v1/node/enroll` is authorized by the enrollment token in its body. The
/// join routes are the same deal one step earlier: a host that has nothing
/// installed yet fetches the script, the installer and the bundle, and the
/// single-use ten-minute token it was handed is what authorizes the enrollment
/// those files lead to. Everything else under `/v1/node/` stays closed.
pub fn node_route_is_public(path: &str) -> bool {
    matches!(
        path,
        "/v1/node/enroll" | "/v1/node/join.sh" | "/v1/node/install.sh" | "/v1/node/bundle"
    ) || path.starts_with("/v1/node/bundle/")
}

/// Enforce Bearer node-credential auth on all `/v1/node/` routes except the
/// ones a node has to reach *before* it has a credential.
pub async fn require_node_auth(
    State(state): State<Arc<AppState>>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = req.uri().path().to_string();
    if path.starts_with("/v1/node/") && !node_route_is_public(&path) {
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
    h.get(agentgrid_common::FENCING_TOKEN_HEADER)
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
    // Plan 6.12 / 2.6: version info is public (health-adjacent — build +
    // contract versions only, no instance data). Used by `ag doctor` and the
    // release smoke test before any login.
    if path == "/v1/version" {
        return false;
    }
    if path.starts_with("/v1/node/") {
        return false;
    }
    // Plan 1.4: GitHub issue webhook carries its own HMAC signature; user
    // JWT is not the right auth model.
    if path.starts_with("/v1/webhooks/") {
        return false;
    }
    // HA peer RPC carries the cluster token and must work before the first
    // user exists (a fresh cluster bootstraps over these routes).
    if path.starts_with("/v1/ha/") {
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
                        Some((username, role)) => {
                            // RBAC (plan 5.2): operators view + approve only.
                            if role != agentgrid_common::ROLE_ADMIN
                                && !operator_allowed(req.method().as_str(), &path)
                            {
                                return Err(StatusCode::FORBIDDEN);
                            }
                            req.extensions_mut().insert(AuthedUser { username, role });
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
    // Finding 7: the first admin is a production credential like any other.
    if !password_is_acceptable(&req.password) {
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
    // short-lived, so timing leakage is not a practical concern). The token
    // stays held under the mutex until the user is created — consuming it
    // first would burn the bootstrap on a transient create_user failure and
    // leave the operator locked out of first-admin creation until restart.
    let mut guard = state.setup_token.lock().await;
    let valid = match guard.as_ref() {
        Some(t) if t.is_live() => t.token == req.setup_token.as_deref().unwrap_or(""),
        _ => false,
    };
    if !valid {
        return Err(StatusCode::FORBIDDEN);
    }
    match state
        .store
        .create_user(&req.username, &req.password, agentgrid_common::ROLE_ADMIN)
        .await
    {
        Ok(true) => {}
        Ok(false) => return Err(StatusCode::CONFLICT),
        Err(e) => {
            tracing::error!("create_user failed: {e}");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    // Consume: the token is single-use, released only after a successful create.
    *guard = None;
    drop(guard);
    // The bootstrapped first user is always an admin.
    let token = state
        .issue_token(&req.username, agentgrid_common::ROLE_ADMIN, 0)
        .map_err(|e| {
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
    crate::net::ClientAddr(client): crate::net::ClientAddr,
    Json(req): Json<LoginRequest>,
) -> Result<(HeaderMap, Json<LoginResponse>), StatusCode> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    // Hard gate, and the only one that runs before Argon2: it caps the CPU an
    // unauthenticated caller can make the control plane burn and stops one
    // address from spraying a password across many accounts. The answer is a
    // generic 429, so it says nothing about whether the username exists.
    {
        let mut rate = state.login_rate.lock().await;
        if !rate.admit(client, now) {
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
    let Some((_, role, sessions_gen)) = user else {
        // Count failures, not attempts: a spent budget turns the answer into
        // a generic 429 but never rejects a correct password, so an attacker
        // cannot lock the real user out of their own account.
        let mut rate = state.login_rate.lock().await;
        return Err(if rate.note_failure(&req.username, now) {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::TOO_MANY_REQUESTS
        });
    };
    // A correct password clears the account's failure budget.
    state.login_rate.lock().await.note_success(&req.username);
    let token = state
        .issue_token(&req.username, &role, sessions_gen)
        .map_err(|e| {
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
    if cookie_secure() {
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
    // A standby is alive but must never receive traffic: only the leader (or
    // a single-mode node) is ready. The role is in memory, so it is checked on
    // every request — a failover must show up immediately.
    if !state.ha.is_leader() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    // The rest of the probe touches the disk and the database. Finding 14: an
    // unauthenticated caller must not be able to turn every request into a
    // file write, so the verdict is reused for a second.
    if let Some(ready) = state.ready_cached() {
        return if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
    }
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
    let ready = state.store.health_check().await && writable;
    state.ready_store(ready);
    if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

#[cfg(test)]
mod rbac_tests {
    use super::*;

    /// Concrete path for a template, so a classification test can call
    /// `operator_allowed` the way a request would.
    fn concrete(template: &str) -> String {
        template
            .split('/')
            .map(|s| {
                if s.starts_with('{') && s.ends_with('}') {
                    "x"
                } else {
                    s
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    /// Finding 8: read access is an allow-list, so a GET that is not declared
    /// is admin-only. These are the two that used to leak.
    #[test]
    fn an_operator_cannot_read_secrets_over_get() {
        assert!(!operator_allowed("GET", "/v1/adapter-env"));
        assert!(!operator_allowed("GET", "/v1/proxies"));
        // A route nobody declared is closed, not open.
        assert!(!operator_allowed("GET", "/v1/credential-store"));
        assert!(!operator_allowed("GET", "/v1/tasks/abc/secret"));
    }

    /// The board the operator is supposed to see still works, including paths
    /// with parameters.
    #[test]
    fn an_operator_can_still_read_the_board() {
        for path in [
            "/v1/tasks",
            "/v1/tasks/3f2a",
            "/v1/tasks/3f2a/events",
            "/v1/tasks/3f2a/artifacts/changes.patch",
            "/v1/nodes",
            "/v1/nodes/node-a/accounts/usage",
            "/v1/workflow-runs/abc/projection",
            "/v1/approvals",
            "/v1/approvals/ap-1",
        ] {
            assert!(operator_allowed("GET", path), "{path} must stay readable");
        }
        // Read does not imply write.
        assert!(!operator_allowed("POST", "/v1/tasks"));
        assert!(!operator_allowed("DELETE", "/v1/tasks/3f2a"));
    }

    /// Every GET the API documents is classified on purpose: the test fails
    /// when a new documented GET is neither readable nor explicitly denied, so
    /// the decision cannot be made by omission.
    #[test]
    fn every_documented_get_route_is_classified() {
        let doc = include_str!("../../../docs/openapi.yaml");
        let mut current: Option<String> = None;
        let mut gets: Vec<String> = Vec::new();
        for line in doc.lines() {
            let trimmed = line.trim_end();
            if let Some(rest) = trimmed.strip_prefix("  /") {
                if !line.starts_with("   ") {
                    current = rest.strip_suffix(':').map(|p| format!("/{p}"));
                    continue;
                }
            }
            if trimmed == "    get:" {
                if let Some(p) = &current {
                    gets.push(p.clone());
                }
            }
        }
        assert!(gets.len() > 50, "openapi parse found only {}", gets.len());
        for path in gets {
            // Public, node-authenticated and HA routes never reach this check.
            if path.starts_with("/health")
                || path == "/metrics"
                || path == "/v1/version"
                || path.starts_with("/v1/node/")
            {
                continue;
            }
            let readable = OPERATOR_READ.contains(&path.as_str());
            let denied = OPERATOR_READ_DENIED.contains(&path.as_str());
            assert!(
                readable ^ denied,
                "{path} must be in exactly one of OPERATOR_READ / OPERATOR_READ_DENIED"
            );
            assert_eq!(
                operator_allowed("GET", &concrete(&path)),
                readable,
                "{path}: the classification and the runtime answer disagree"
            );
        }
    }
}
