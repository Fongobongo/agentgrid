//! Control plane for agentgrid.
//!
//! HTTP surface (`/v1`) and long-poll scheduler are stable; the backing store
//! is SQLite (see [`store`]). Stage 1 used an in-memory map — swapped for
//! persistence in Stage 2.1.

pub mod store;
pub mod workflow;

use anyhow::Context;
use std::sync::Arc;
use std::time::Instant;

use agentgrid_common::{
    AppendMessageRequest, ApprovalEvent, ApprovalView, CancelState, CompleteAttemptRequest,
    CreateAgentSessionRequest, CreateConversationRequest, CreateRepositoryRequest,
    CreateTaskRequest, CreateWorkflowRequest, CreateWorkflowRunRequest, EnrollRequest,
    EnrollResponse, EnrollTokenResponse, EventsQuery, HeartbeatRequest, IngestEventsRequest,
    LoginRequest, LoginResponse, PollRequest, PollResponse, RepositoryView, SetupRequest,
    TaskEligibility, TaskView, UploadArtifactRequest, WorkflowProjection, WorkflowRun,
    WorkflowRunWithSteps, WorkflowTemplate,
};
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Extension, Path, Query, State},
    http::{header, HeaderMap, Request, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use futures_core::Stream;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use store::Store;
use tokio::sync::Notify;
use uuid::Uuid;

const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// JWT claims for user sessions (Stage 4.1).
#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    exp: usize,
}

/// Stage 2.5: the cookie name carrying the session JWT, set HttpOnly so the
/// browser cannot read it (no XSS token theft) with SameSite=Strict (CSRF
/// guard). `Secure` is added only when `AGENTGRID_COOKIE_SECURE=1` so local
/// plaintext dev keeps working.
const AUTH_COOKIE: &str = "agentgrid_token";

/// Extract a session JWT from a request: an `Authorization: Bearer` header
/// (non-browser clients: CLI, gateway, node) or the `agentgrid_token` cookie
/// (browser fetch with `credentials: include`).
fn auth_token_from_headers(headers: &HeaderMap) -> Option<String> {
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
fn auth_cookie_header(token: &str) -> String {
    let secure = std::env::var("AGENTGRID_COOKIE_SECURE").as_deref() == Ok("1");
    let mut v = format!("{AUTH_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200");
    if secure {
        v.push_str("; Secure");
    }
    v
}

pub struct AppState {
    pub store: Store,
    assignment_notify: Arc<Notify>,
    jwt_secret: Vec<u8>,
    /// Directory with the built web UI (Stage 4.3). Served as static files;
    /// `None` disables the UI.
    web_root: Option<std::path::PathBuf>,
    /// Request size ceilings (Stage 5.1).
    limits: Limits,
    /// Database file path (for SQLite size metrics, Stage 5.2).
    db_path: String,
    /// Brute-force protection on `/v1/auth/login` (Stage 2.5).
    login_rate: Arc<tokio::sync::Mutex<LoginRate>>,
}

/// Request size ceilings (Stage 5.1). Overridable via env; defaults:
/// prompt 64 KiB, event payload 1 MiB, artifact 50 MiB.
struct Limits {
    prompt: usize,
    event: usize,
    artifact: usize,
}

/// Sliding-window brute-force limiter for the login endpoint (Stage 2.5).
/// Keyed globally per control-plane instance; a generic 429 (not a per-user
/// signal) is returned when the budget is spent, so it cannot be used to
/// enumerate which usernames exist.
struct LoginRate {
    window_start: i64,
    count: u32,
    max: u32,
    window_secs: i64,
}

impl LoginRate {
    fn new() -> Self {
        Self {
            window_start: 0,
            count: 0,
            max: 10,
            window_secs: 60,
        }
    }
    /// Record an attempt; returns false once the per-window budget is spent.
    fn check_and_record(&mut self, now: i64) -> bool {
        if now - self.window_start >= self.window_secs {
            self.window_start = now;
            self.count = 0;
        }
        self.count += 1;
        self.count <= self.max
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// User identity established by [`require_user_auth`]; read by user handlers.
#[derive(Clone)]
struct AuthedUser {
    username: String,
}

impl AppState {
    /// Open (or create) the SQLite database at `db_path` and return shared state.
    pub async fn open(db_path: &str) -> anyhow::Result<Arc<Self>> {
        let store = Store::open(db_path).await?;
        let jwt_secret = match std::env::var("AGENTGRID_JWT_SECRET") {
            Ok(s) => s.into_bytes(),
            Err(_) => {
                // Stage 2.5: a random-per-start secret invalidates previously
                // issued node tokens after a restart. Require a stable secret in
                // production; warn loudly when one is not configured.
                tracing::warn!(
                    "AGENTGRID_JWT_SECRET unset: using a random secret for this run; \
                     existing node tokens will not survive a restart"
                );
                use rand::Rng;
                rand::thread_rng().gen::<[u8; 32]>().to_vec()
            }
        };
        // Bootstrap the first user from env (one-time) so a fresh install is
        // not left in its open window.
        if let (Ok(u), Ok(p)) = (
            std::env::var("AGENTGRID_BOOTSTRAP_USER"),
            std::env::var("AGENTGRID_BOOTSTRAP_PASSWORD"),
        ) {
            if store.user_count().await? == 0 {
                store.create_user(&u, &p).await?;
            }
        }
        let web_root = std::env::var("AGENTGRID_WEB_ROOT")
            .map(std::path::PathBuf::from)
            .ok()
            .or_else(|| {
                std::env::current_exe().ok().and_then(|p| {
                    p.parent().map(|d| {
                        let dist = d.join("web").join("dist");
                        if dist.join("index.html").exists() {
                            dist
                        } else {
                            d.join("web")
                        }
                    })
                })
            });
        let limits = Limits {
            prompt: env_usize("AGENTGRID_MAX_PROMPT_KB", 64) * 1024,
            event: env_usize("AGENTGRID_MAX_EVENT_KB", 1024) * 1024,
            artifact: env_usize("AGENTGRID_MAX_ARTIFACT_MB", 50) * 1024 * 1024,
        };
        Ok(Arc::new(Self {
            store,
            assignment_notify: Arc::new(Notify::new()),
            jwt_secret,
            web_root,
            limits,
            db_path: db_path.to_string(),
            login_rate: Arc::new(tokio::sync::Mutex::new(LoginRate::new())),
        }))
    }

    /// Open a fresh temporary database (used by tests).
    pub async fn open_temp() -> anyhow::Result<Arc<Self>> {
        let p = std::env::temp_dir().join(format!("ag-test-{}.db", Uuid::new_v4()));
        Self::open(p.to_str().unwrap()).await
    }

    /// Issue a 12h JWT for `username` (Stage 4.1).
    fn issue_token(&self, username: &str) -> anyhow::Result<String> {
        let exp = (chrono::Utc::now() + chrono::Duration::hours(12)).timestamp() as usize;
        let claims = Claims {
            sub: username.to_string(),
            exp,
        };
        Ok(encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(&self.jwt_secret),
        )?)
    }

    /// Validate a JWT and return the username, or None.
    fn verify_token(&self, token: &str) -> Option<String> {
        decode::<Claims>(
            token,
            &DecodingKey::from_secret(&self.jwt_secret),
            &Validation::default(),
        )
        .ok()
        .map(|d| d.claims.sub)
    }
}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        .route("/metrics", get(metrics))
        .route("/v1/tasks", post(create_task).get(list_tasks))
        .route("/v1/tasks/{id}", get(show_task))
        .route("/v1/tasks/{id}/events", get(get_events))
        .route("/v1/tasks/{id}/events/stream", get(events_stream))
        .route("/v1/tasks/{id}/cancel", post(cancel_task_handler))
        .route("/v1/tasks/{id}/retry", post(retry_task_handler))
        .route("/v1/tasks/{id}/eligibility", get(task_eligibility_handler))
        .route("/v1/approvals", get(list_approvals_handler))
        .route("/v1/approvals/{id}", get(get_approval_handler))
        .route("/v1/approvals/{id}/allow", post(allow_approval_handler))
        .route("/v1/approvals/{id}/deny", post(deny_approval_handler))
        .route(
            "/v1/tasks/{id}/approvals",
            post(create_approval_for_task_handler),
        )
        .route("/v1/auth/setup", post(auth_setup))
        .route("/v1/auth/login", post(auth_login))
        .route("/v1/auth/logout", post(auth_logout))
        .route("/v1/policy/evaluate", post(evaluate_policy))
        .route("/v1/skills", get(list_skills_trust_handler))
        .route("/v1/skills/{name}", get(get_skill_trust_handler))
        .route("/v1/skills/{name}/trust", post(trust_skill_handler))
        .route("/v1/skills/{name}/untrust", post(untrust_skill_handler))
        .route("/v1/profiles", get(list_profiles_handler))
        .route("/v1/profiles/{id}", get(get_profile_handler))
        .route("/v1/profiles/{id}", post(create_profile_handler))
        .route("/v1/profiles/{id}/activate", post(activate_profile_handler))
        .route("/v1/admin/backup", post(admin_backup))
        .route("/v1/nodes", get(list_nodes))
        .route("/v1/nodes/enrollment-token", post(create_enrollment_token))
        .route("/v1/nodes/{id}", delete(revoke_node))
        .route(
            "/v1/repositories",
            post(create_repository).get(list_repositories),
        )
        .route("/v1/node/enroll", post(enroll))
        .route("/v1/node/poll", post(poll))
        .route("/v1/node/heartbeat", post(heartbeat))
        .route("/v1/node/attempts/{id}/cancel", get(attempt_cancel_handler))
        .route("/v1/node/attempts/{id}/events", post(ingest_events))
        .route("/v1/node/attempts/{id}/complete", post(complete_attempt))
        .route("/v1/node/attempts/{id}/ack", post(ack_attempt_handler))
        .route(
            "/v1/node/attempts/{id}/session",
            post(create_agent_session_handler),
        )
        .route("/v1/node/attempts/{id}/artifacts", post(upload_artifact))
        .route("/v1/tasks/{id}/artifacts/{name}", get(get_artifact))
        .route("/v1/conversations", post(create_conversation))
        .route("/v1/conversations/{id}", get(show_conversation))
        .route(
            "/v1/conversations/{id}/messages",
            post(append_conversation_message).get(list_conversation_messages),
        )
        .route("/v1/workflows", post(create_workflow).get(list_workflows))
        .route("/v1/workflows/{id}", get(show_workflow))
        .route("/v1/workflows/{id}/runs", post(create_workflow_run))
        .route("/v1/workflow-runs", get(list_workflow_runs))
        .route("/v1/workflow-runs/{id}", get(show_workflow_run))
        .route(
            "/v1/workflow-runs/{id}/projection",
            get(workflow_run_projection),
        )
        .route("/v1/workflow-runs/{id}/tick", post(tick_workflow_run))
        .route(
            "/v1/workflow-runs/{id}/cancel",
            post(cancel_workflow_run_handler),
        )
        .layer(DefaultBodyLimit::max(state.limits.artifact))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_user_auth,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_node_auth,
        ))
        .fallback(static_fallback)
        .with_state(state)
}

/// Serve the built web UI (Stage 4.3). Unknown non-API paths fall back to
/// `index.html`; missing files under `/v1/` return 404.
async fn static_fallback(State(state): State<Arc<AppState>>, uri: Uri) -> Response {
    use axum::http::header::CONTENT_TYPE;
    let root = match &state.web_root {
        Some(r) => r.clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let rel = uri.path().trim_start_matches('/');
    let fs_path = if rel.is_empty() {
        root.join("index.html")
    } else {
        root.join(rel)
    };
    if fs_path != root && !fs_path.starts_with(&root) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match tokio::fs::read(&fs_path).await {
        Ok(bytes) => {
            let ct = content_type(&fs_path);
            ([(CONTENT_TYPE, ct)], bytes).into_response()
        }
        Err(_) => {
            if uri.path().starts_with("/v1/") {
                return StatusCode::NOT_FOUND.into_response();
            }
            match tokio::fs::read(root.join("index.html")).await {
                Ok(b) => {
                    ([(CONTENT_TYPE, "text/html; charset=utf-8".to_string())], b).into_response()
                }
                Err(_) => StatusCode::NOT_FOUND.into_response(),
            }
        }
    }
}

fn content_type(path: &std::path::Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Node identity established by [`require_node_auth`]; read by node handlers.
#[derive(Clone)]
struct AuthedNode {
    node_id: String,
}

/// Enforce Bearer node-credential auth on all `/v1/node/` routes except enroll.
async fn require_node_auth(
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

async fn health_live() -> StatusCode {
    StatusCode::OK
}

/// Whether a path requires a user JWT (Stage 4.1). Node auth (`/v1/node/*`)
/// and the auth endpoints themselves are exempt; health/metrics are public.
fn user_protected(path: &str) -> bool {
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

/// Require a valid user JWT on user-facing routes, except during the open
/// bootstrap window (no users yet). Node routes are handled by
/// [`require_node_auth`] and are skipped here.
async fn require_user_auth(
    State(state): State<Arc<AppState>>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = req.uri().path().to_string();
    if user_protected(&path) {
        let open = state
            .store
            .user_count()
            .await
            .map(|c| c == 0)
            .unwrap_or(true);
        if !open {
            match auth_token_from_headers(req.headers()).and_then(|t| state.verify_token(&t)) {
                Some(u) => {
                    req.extensions_mut().insert(AuthedUser { username: u });
                }
                None => return Err(StatusCode::UNAUTHORIZED),
            }
        }
    }
    Ok(next.run(req).await)
}

async fn auth_setup(
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

async fn auth_login(
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
        if !rate.check_and_record(now) {
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

async fn auth_logout() -> (HeaderMap, StatusCode) {
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

async fn health_ready(State(state): State<Arc<AppState>>) -> StatusCode {
    let dir = std::path::Path::new(&state.db_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let probe = dir.join(".agentgrid-health-probe");
    let writable = std::fs::write(&probe, b"ok").is_ok();
    let _ = std::fs::remove_file(&probe);
    if state.store.health_check().await && writable {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn cancel_workflow_run_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> StatusCode {
    match state.store.cancel_workflow_run(&id).await {
        Ok(true) => StatusCode::OK,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("cancel_workflow_run failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
/// Stage 2.5: compact-copy the database to `path` via `VACUUM INTO`.
/// User-authenticated (the global user-auth middleware covers it).
async fn admin_backup(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BackupRequest>,
) -> StatusCode {
    match state.store.backup_to(&req.path).await {
        Ok(()) => StatusCode::OK,
        Err(e) => {
            tracing::error!("backup failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[derive(serde::Deserialize)]
struct BackupRequest {
    path: String,
}
/// its verdict. Fail-closed: a provider error yields `ask`, never `allow`.
async fn evaluate_policy(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EvaluatePolicyRequest>,
) -> Json<agentgrid_common::PolicyVerdict> {
    let level = req.autonomy.unwrap_or_default();
    let verdict = agentgrid_common::BuiltinPolicyProvider::new()
        .evaluate_with(level, &req.command, &req.cwd)
        .unwrap_or_else(|e| agentgrid_common::PolicyVerdict::fail_closed(&e.0));
    // Fail-closed audit: every policy decision is recorded so dangerous commands
    // are never silent.
    let payload = serde_json::to_string(&verdict).unwrap_or_else(|_| "{}".to_string());
    let _ = state
        .store
        .audit(
            "system",
            None,
            "policy.evaluate",
            Some(&req.command),
            Some(&payload),
        )
        .await;
    Json(verdict)
}

// ---- Skill trust (Stage 9.2) ----

/// Query param for listing trust: `?source=project` filters by source tier.
#[derive(serde::Deserialize)]
struct SkillTrustQuery {
    source: Option<String>,
}

async fn list_skills_trust_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SkillTrustQuery>,
) -> Result<Json<Vec<agentgrid_common::SkillTrustView>>, StatusCode> {
    let rows = state.store.list_skill_trust().await.map_err(|e| {
        tracing::error!("list_skill_trust failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(match q.source {
        Some(s) => rows.into_iter().filter(|r| r.source == s).collect(),
        None => rows,
    }))
}

async fn get_skill_trust_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SkillTrustQuery>,
    Path(name): Path<String>,
) -> Result<Json<agentgrid_common::SkillTrustView>, StatusCode> {
    let source = q.source.unwrap_or_else(|| "project".to_string());
    state
        .store
        .get_skill_trust(&name, &source)
        .await
        .map(Json)
        .map_err(|e| {
            tracing::error!("get_skill_trust failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn trust_skill_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Query(q): Query<SkillTrustQuery>,
    Path(name): Path<String>,
) -> StatusCode {
    set_skill_trust(state, auth, &name, q.source.as_deref(), true).await
}

async fn untrust_skill_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Query(q): Query<SkillTrustQuery>,
    Path(name): Path<String>,
) -> StatusCode {
    set_skill_trust(state, auth, &name, q.source.as_deref(), false).await
}

async fn set_skill_trust(
    state: Arc<AppState>,
    auth: Option<Extension<AuthedUser>>,
    name: &str,
    source: Option<&str>,
    trusted: bool,
) -> StatusCode {
    let actor = auth
        .as_ref()
        .map(|e| e.0.username.as_str())
        .unwrap_or("system");
    let source = source.unwrap_or("project");
    if let Err(e) = state
        .store
        .set_skill_trust(name, source, trusted, actor)
        .await
    {
        tracing::error!("set_skill_trust failed: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    let _ = state
        .store
        .audit(
            actor,
            None,
            "skill.trust",
            Some(&format!("{name}/{source}")),
            Some(if trusted { "trusted" } else { "untrusted" }),
        )
        .await;
    StatusCode::OK
}

// ---- Agent profiles (Stage 13) ----

async fn list_profiles_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<String>>, StatusCode> {
    state.store.list_profiles().await.map(Json).map_err(|e| {
        tracing::error!("list_profiles failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

async fn get_profile_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<agentgrid_common::AgentProfile>>, StatusCode> {
    state
        .store
        .list_profile_revisions(&id)
        .await
        .map(Json)
        .map_err(|e| {
            tracing::error!("get_profile failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn create_profile_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Path(id): Path<String>,
    Json(body): Json<agentgrid_common::AgentProfileCreate>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let actor = auth
        .as_ref()
        .map(|e| e.0.username.as_str())
        .unwrap_or("system");
    match state.store.create_profile_revision(&id, &body, actor).await {
        Ok(rev) => {
            let _ = state
                .store
                .audit(
                    actor,
                    None,
                    "profile.create",
                    Some(&format!("{id}/{rev}")),
                    None,
                )
                .await;
            Ok(Json(serde_json::json!({ "id": id, "revision": rev })))
        }
        Err(e) => {
            tracing::error!("create_profile failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn activate_profile_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Path(id): Path<String>,
    Json(body): Json<agentgrid_common::ActivateProfile>,
) -> StatusCode {
    let actor = auth
        .as_ref()
        .map(|e| e.0.username.as_str())
        .unwrap_or("system");
    if let Err(e) = state.store.activate_profile(&id, body.revision).await {
        tracing::error!("activate_profile failed: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    let _ = state
        .store
        .audit(
            actor,
            None,
            "profile.activate",
            Some(&format!("{id}/{}", body.revision)),
            None,
        )
        .await;
    StatusCode::OK
}

#[derive(serde::Deserialize)]
struct EvaluatePolicyRequest {
    command: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    autonomy: Option<agentgrid_common::AutonomyLevel>,
}

async fn metrics(State(state): State<Arc<AppState>>) -> (StatusCode, axum::response::Response) {
    use axum::response::IntoResponse;
    let nodes = match state.store.list_nodes().await {
        Ok(n) => n,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "".into_response()),
    };
    let tasks = match state.store.list_tasks().await {
        Ok(t) => t,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "".into_response()),
    };
    let attempts = state.store.count_attempts().await.unwrap_or(0);

    let mut node_status = std::collections::HashMap::<String, u64>::new();
    for n in &nodes {
        *node_status.entry(format!("{}", n.status)).or_insert(0) += 1;
    }
    let mut task_status = std::collections::HashMap::<String, u64>::new();
    for t in &tasks {
        *task_status.entry(format!("{}", t.status)).or_insert(0) += 1;
    }

    // Task duration histogram + terminal outcome counters (Stage 5.2).
    let mut buckets: [(u64, u64); 5] = [(60, 0), (300, 0), (1800, 0), (3600, 0), (u64::MAX, 0)];
    let mut dur_sum = 0u64;
    let mut dur_count = 0u64;
    let mut outcome: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for t in &tasks {
        if let (Some(f), c) = (t.finished_at.as_deref(), t.created_at.as_str()) {
            if let (Ok(fdt), Ok(cdt)) = (
                chrono::DateTime::parse_from_rfc3339(f),
                chrono::DateTime::parse_from_rfc3339(c),
            ) {
                let secs = (fdt - cdt).num_seconds().max(0) as u64;
                dur_sum += secs;
                dur_count += 1;
                for b in buckets.iter_mut() {
                    if secs <= b.0 {
                        b.1 += 1;
                    }
                }
            }
        }
        let st = format!("{}", t.status);
        if st == "succeeded" || st == "failed" || st == "cancelled" {
            *outcome.entry(st).or_insert(0) += 1;
        }
    }

    let mut s = String::new();
    s.push_str("# HELP agentgrid_nodes Nodes by status.\n");
    s.push_str("# TYPE agentgrid_nodes gauge\n");
    for (st, c) in &node_status {
        s.push_str(&format!("agentgrid_nodes{{status=\"{st}\"}} {c}\n"));
    }
    s.push_str("# HELP agentgrid_tasks Tasks by status.\n");
    s.push_str("# TYPE agentgrid_tasks gauge\n");
    for (st, c) in &task_status {
        s.push_str(&format!("agentgrid_tasks{{status=\"{st}\"}} {c}\n"));
    }
    s.push_str("# HELP agentgrid_attempts_total Total attempts.\n");
    s.push_str("# TYPE agentgrid_attempts_total counter\n");
    s.push_str(&format!("agentgrid_attempts_total {attempts}\n"));

    s.push_str("# HELP agentgrid_task_duration_seconds Task duration (finished tasks).\n");
    s.push_str("# TYPE agentgrid_task_duration_seconds histogram\n");
    for (le, c) in &buckets {
        let le_s = if *le == u64::MAX {
            "+Inf".to_string()
        } else {
            le.to_string()
        };
        s.push_str(&format!(
            "agentgrid_task_duration_seconds_bucket{{le=\"{le_s}\"}} {c}\n"
        ));
    }
    s.push_str(&format!("agentgrid_task_duration_seconds_sum {dur_sum}\n"));
    s.push_str(&format!(
        "agentgrid_task_duration_seconds_count {dur_count}\n"
    ));

    s.push_str("# HELP agentgrid_tasks_total Terminal task outcomes (cumulative).\n");
    s.push_str("# TYPE agentgrid_tasks_total counter\n");
    for (st, c) in &outcome {
        s.push_str(&format!("agentgrid_tasks_total{{status=\"{st}\"}} {c}\n"));
    }

    s.push_str("# HELP agentgrid_node_free_disk_mb Free disk reported via heartbeat.\n");
    s.push_str("# TYPE agentgrid_node_free_disk_mb gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_free_disk_mb{{node=\"{}\"}} {}\n",
            n.name, n.free_disk_mb
        ));
    }
    s.push_str("# HELP agentgrid_node_load_avg Load average reported via heartbeat.\n");
    s.push_str("# TYPE agentgrid_node_load_avg gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_load_avg{{node=\"{}\"}} {}\n",
            n.name, n.load_avg
        ));
    }

    s.push_str("# HELP agentgrid_sqlite_db_bytes Main database file size in bytes.\n");
    s.push_str("# TYPE agentgrid_sqlite_db_bytes gauge\n");
    let db_bytes = std::fs::metadata(&state.db_path)
        .map(|m| m.len())
        .unwrap_or(0);
    s.push_str(&format!("agentgrid_sqlite_db_bytes {db_bytes}\n"));
    s.push_str("# HELP agentgrid_sqlite_wal_bytes WAL file size in bytes.\n");
    s.push_str("# TYPE agentgrid_sqlite_wal_bytes gauge\n");
    let wal_bytes = std::fs::metadata(format!("{}-wal", state.db_path))
        .map(|m| m.len())
        .unwrap_or(0);
    s.push_str(&format!("agentgrid_sqlite_wal_bytes {wal_bytes}\n"));

    s.push_str(
        "# HELP agentgrid_scheduler_latency_ms Last scheduler latency: queued→assigned in ms.\n",
    );
    s.push_str("# TYPE agentgrid_scheduler_latency_ms gauge\n");
    s.push_str(&format!(
        "agentgrid_scheduler_latency_ms {}\n",
        state
            .store
            .scheduler_latency_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(
        "# HELP agentgrid_scheduler_assignments_total Total assignments made by the scheduler.\n",
    );
    s.push_str("# TYPE agentgrid_scheduler_assignments_total counter\n");
    s.push_str(&format!(
        "agentgrid_scheduler_assignments_total {}\n",
        state
            .store
            .scheduler_assignments
            .load(std::sync::atomic::Ordering::Relaxed)
    ));

    s.push_str(
        "# HELP agentgrid_sqlite_checkpoint_ms Last wal_checkpoint(TRUNCATE) duration in ms.\n",
    );
    s.push_str("# TYPE agentgrid_sqlite_checkpoint_ms gauge\n");
    s.push_str(&format!(
        "agentgrid_sqlite_checkpoint_ms {}\n",
        state
            .store
            .checkpoint_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(
        "# HELP agentgrid_sqlite_busy_total Cumulative SQLITE_BUSY/locked-class failures.\n",
    );
    s.push_str("# TYPE agentgrid_sqlite_busy_total counter\n");
    s.push_str(&format!(
        "agentgrid_sqlite_busy_total {}\n",
        state
            .store
            .sqlite_busy
            .load(std::sync::atomic::Ordering::Relaxed)
    ));

    (
        StatusCode::OK,
        (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; version=0.0.4",
            )],
            s,
        )
            .into_response(),
    )
}

async fn create_task(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Json(req): Json<CreateTaskRequest>,
) -> Result<(StatusCode, Json<TaskView>), StatusCode> {
    if req.prompt.len() > state.limits.prompt {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    match state.store.create_task(&req).await {
        Ok(view) => {
            state.assignment_notify.notify_waiters();
            let _ = state
                .store
                .audit(
                    "user",
                    auth.as_ref().map(|e| e.0.username.as_str()),
                    "task.create",
                    Some(&view.id),
                    None,
                )
                .await;
            Ok((StatusCode::CREATED, Json(view)))
        }
        Err(e) => {
            tracing::error!("create_task failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn list_tasks(State(state): State<Arc<AppState>>) -> Json<Vec<TaskView>> {
    match state.store.list_tasks().await {
        Ok(t) => Json(t),
        Err(e) => {
            tracing::error!("list_tasks failed: {e}");
            Json(vec![])
        }
    }
}

async fn show_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<TaskView>, StatusCode> {
    state
        .store
        .show_task(&id)
        .await
        .map_err(|e| {
            tracing::error!("show_task failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn task_eligibility_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<TaskEligibility>, StatusCode> {
    state
        .store
        .task_eligibility(&id)
        .await
        .map_err(|e| {
            tracing::error!("task_eligibility failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

// ----- workflows (Stage 7.2) -----

async fn create_workflow(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<WorkflowTemplate>), StatusCode> {
    let is_yaml = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("yaml") || v.contains("yml"))
        .unwrap_or(false);
    let req: CreateWorkflowRequest = if is_yaml {
        let text = String::from_utf8_lossy(&body);
        let t = WorkflowTemplate::from_yaml(&text).map_err(|e| {
            tracing::error!("workflow yaml parse failed: {e}");
            StatusCode::BAD_REQUEST
        })?;
        t.validate_dag().map_err(|e| {
            tracing::warn!("workflow DAG invalid: {e}");
            StatusCode::BAD_REQUEST
        })?;
        CreateWorkflowRequest {
            name: t.name,
            steps: t.steps,
            context: None,
        }
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            tracing::error!("workflow json parse failed: {e}");
            StatusCode::BAD_REQUEST
        })?
    };
    // Validate the graph (ADR 0004) on the JSON path too: YAML is checked above,
    // JSON-built templates go through the same invariant so a malformed graph
    // never reaches the scheduler.
    WorkflowTemplate {
        id: String::new(),
        name: req.name.clone(),
        steps: req.steps.clone(),
        created_at: String::new(),
    }
    .validate_dag()
    .map_err(|e| {
        tracing::warn!("workflow DAG invalid: {e}");
        StatusCode::BAD_REQUEST
    })?;
    state
        .store
        .create_workflow_template(&req.name, &req.steps)
        .await
        .map(|t| (StatusCode::CREATED, Json(t)))
        .map_err(|e| {
            tracing::error!("create_workflow failed: {e}");
            StatusCode::BAD_REQUEST
        })
}

async fn list_workflows(State(state): State<Arc<AppState>>) -> Json<Vec<WorkflowTemplate>> {
    match state.store.list_workflow_templates().await {
        Ok(t) => Json(t),
        Err(e) => {
            tracing::error!("list_workflows failed: {e}");
            Json(vec![])
        }
    }
}

async fn show_workflow(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<WorkflowTemplate>, StatusCode> {
    match state.store.get_workflow_template(&id).await {
        Ok(Some(t)) => Ok(Json(t)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("show_workflow failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn create_workflow_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<CreateWorkflowRunRequest>,
) -> Result<(StatusCode, Json<WorkflowRun>), StatusCode> {
    state
        .store
        .create_workflow_run(
            &id,
            req.context.as_deref(),
            req.repository.as_deref(),
            req.base_commit.as_deref(),
        )
        .await
        .map(|r| (StatusCode::CREATED, Json(r)))
        .map_err(|e| {
            tracing::error!("create_workflow_run failed: {e}");
            StatusCode::BAD_REQUEST
        })
}

async fn list_workflow_runs(State(state): State<Arc<AppState>>) -> Json<Vec<WorkflowRun>> {
    match state.store.list_workflow_runs().await {
        Ok(r) => Json(r),
        Err(e) => {
            tracing::error!("list_workflow_runs failed: {e}");
            Json(vec![])
        }
    }
}

async fn show_workflow_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<WorkflowRunWithSteps>, StatusCode> {
    let run = state.store.get_workflow_run(&id).await;
    let steps = state.store.get_workflow_run_steps(&id).await;
    match (run, steps) {
        (Ok(Some(r)), Ok(s)) => Ok(Json(WorkflowRunWithSteps { run: r, steps: s })),
        (Ok(None), _) => Err(StatusCode::NOT_FOUND),
        _ => {
            tracing::error!("show_workflow_run failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Stage 8 ACP plan projection: live roles/steps/nodes/verdicts for a run.
async fn workflow_run_projection(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<WorkflowProjection>, StatusCode> {
    match state.store.get_workflow_run_projection(&id).await {
        Ok(Some(p)) => Ok(Json(p)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("workflow_run_projection failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn tick_workflow_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<WorkflowRunWithSteps>, StatusCode> {
    if state.store.tick_workflow_run(&id).await.is_err() {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    // Wake the scheduler so freshly-created step tasks get assigned promptly.
    state.assignment_notify.notify_waiters();
    match state.store.get_workflow_run(&id).await {
        Ok(Some(r)) => {
            let steps = state
                .store
                .get_workflow_run_steps(&id)
                .await
                .unwrap_or_default();
            Ok(Json(WorkflowRunWithSteps { run: r, steps }))
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn list_nodes(State(state): State<Arc<AppState>>) -> Json<Vec<agentgrid_common::NodeView>> {
    match state.store.list_nodes().await {
        Ok(n) => Json(n),
        Err(e) => {
            tracing::error!("list_nodes failed: {e}");
            Json(vec![])
        }
    }
}

async fn create_enrollment_token(
    State(state): State<Arc<AppState>>,
) -> Result<Json<EnrollTokenResponse>, StatusCode> {
    state
        .store
        .create_enrollment_token()
        .await
        .map(|(token, expires_at)| Json(EnrollTokenResponse { token, expires_at }))
        .map_err(|e| {
            tracing::error!("create_enrollment_token failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn enroll(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EnrollRequest>,
) -> (StatusCode, Json<Option<EnrollResponse>>) {
    match state.store.enroll_node(&req).await {
        Ok(Some(r)) => {
            if agentgrid_common::is_incompatible_protocol(&req.protocol_version) {
                let _ = state.store.set_node_degraded(&r.node_id).await;
            }
            (StatusCode::OK, Json(Some(r)))
        }
        Ok(None) => (StatusCode::BAD_REQUEST, Json(None)),
        Err(e) => {
            tracing::error!("enroll failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(None))
        }
    }
}

async fn heartbeat(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Json(req): Json<HeartbeatRequest>,
) -> StatusCode {
    if agentgrid_common::is_incompatible_protocol(&req.protocol_version) {
        let _ = state.store.set_node_degraded(&auth.node_id).await;
    }
    match state.store.heartbeat(&auth.node_id, &req).await {
        Ok(true) => StatusCode::OK,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("heartbeat failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn revoke_node(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> StatusCode {
    match state.store.revoke_node(&id).await {
        Ok(true) => StatusCode::OK,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("revoke_node failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn create_repository(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Json(req): Json<CreateRepositoryRequest>,
) -> (StatusCode, Json<RepositoryView>) {
    match state.store.create_repository(&req).await {
        Ok(v) => {
            let _ = state
                .store
                .audit(
                    "user",
                    auth.as_ref().map(|e| e.0.username.as_str()),
                    "repo.add",
                    Some(&v.id),
                    None,
                )
                .await;
            (StatusCode::CREATED, Json(v))
        }
        Err(e) => {
            tracing::error!("create_repository failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(RepositoryView {
                    id: String::new(),
                    name: String::new(),
                    git_url: String::new(),
                    default_branch: String::new(),
                    validation_command: None,
                    created_at: String::new(),
                }),
            )
        }
    }
}

async fn list_repositories(State(state): State<Arc<AppState>>) -> Json<Vec<RepositoryView>> {
    match state.store.list_repositories().await {
        Ok(r) => Json(r),
        Err(e) => {
            tracing::error!("list_repositories failed: {e}");
            Json(vec![])
        }
    }
}

// ----- conversations (stateful multi-turn chat routed to an agent) -----

async fn create_conversation(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateConversationRequest>,
) -> (StatusCode, Json<agentgrid_common::Conversation>) {
    match state
        .store
        .create_conversation(&req.adapter, &req.repository)
        .await
    {
        Ok(c) => (StatusCode::CREATED, Json(c)),
        Err(e) => {
            tracing::error!("create_conversation failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(agentgrid_common::Conversation {
                    id: String::new(),
                    adapter: String::new(),
                    repository: String::new(),
                    created_at: String::new(),
                }),
            )
        }
    }
}

async fn show_conversation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<agentgrid_common::Conversation>, StatusCode> {
    state
        .store
        .get_conversation(&id)
        .await
        .map_err(|e| {
            tracing::error!("get_conversation failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Compose the conversation history into a prompt the agent receives, so any
/// node picking the task up sees the full shared context. Format is a simple
/// transcript: `user:` / `assistant:` lines.
fn compose_conversation_prompt(
    messages: &[agentgrid_common::ConversationMessage],
    new_user: &str,
) -> String {
    let mut s = String::new();
    for m in messages {
        s.push_str(m.role.as_str());
        s.push_str(": ");
        s.push_str(&m.content);
        s.push('\n');
    }
    s.push_str("user: ");
    s.push_str(new_user);
    s
}

/// Append a user message and create a task carrying the composed conversation
/// prompt. The task is assigned by the scheduler to any node serving
/// `adapter`+`repository`. Returns the task id so the gateway can stream the
/// answer.
async fn append_conversation_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AppendMessageRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    let conv = state
        .store
        .get_conversation(&id)
        .await
        .map_err(|e| {
            tracing::error!("get_conversation failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    let history = state
        .store
        .list_conversation_messages(&id)
        .await
        .map_err(|e| {
            tracing::error!("list_conversation_messages failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let prompt = compose_conversation_prompt(&history, &req.content);
    // Stage 11.5: if a prior turn finished an ACP session, resume it so the
    // agent does not re-process the transcript from scratch.
    let parent_acp_session_id = state
        .store
        .last_conversation_acp_session(&id)
        .await
        .map_err(|e| {
            tracing::warn!("last_conversation_acp_session failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let task_req = CreateTaskRequest {
        prompt,
        repository: conv.repository.clone(),
        adapter: conv.adapter.clone(),
        requested_node_id: None,
        timeout_secs: None,
        validation_command: None,
        base_commit: None,
        parent_acp_session_id,
    };
    let task = state.store.create_task(&task_req).await.map_err(|e| {
        tracing::error!("create_task for conversation failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    state
        .store
        .append_conversation_message(&id, "user", &req.content, Some(&task.id))
        .await
        .map_err(|e| {
            tracing::error!("append user message failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"task_id": task.id, "conversation_id": id})),
    ))
}

async fn list_conversation_messages(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<agentgrid_common::ConversationMessage>>, StatusCode> {
    state
        .store
        .list_conversation_messages(&id)
        .await
        .map(Json)
        .map_err(|e| {
            tracing::error!("list_conversation_messages failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn get_artifact(
    State(state): State<Arc<AppState>>,
    Path((task_id, name)): Path<(String, String)>,
) -> Result<String, StatusCode> {
    // Stage 2.2: a crafted name (../, absolute, ...) must not traverse out of
    // the artifact root via store::read_artifact's join. Reject as 404 so a
    // denial does not disclose whether the task/artifact exists.
    if !is_safe_artifact_name(&name) {
        return Err(StatusCode::NOT_FOUND);
    }
    match state.store.read_artifact(&task_id, &name).await {
        Ok(Some(s)) => Ok(s),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("read_artifact failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn upload_artifact(
    State(state): State<Arc<AppState>>,
    Path(attempt_id): Path<String>,
    Json(req): Json<UploadArtifactRequest>,
) -> StatusCode {
    if req.content.len() > state.limits.artifact {
        return StatusCode::PAYLOAD_TOO_LARGE;
    }
    // Stage 2.2: never let a crafted name escape the artifact root
    // (../../etc/passwd, absolute paths, separators).
    if !is_safe_artifact_name(&req.name) {
        return StatusCode::BAD_REQUEST;
    }
    match state.store.save_artifact(&attempt_id, &req).await {
        Ok(()) => StatusCode::OK,
        Err(e) => {
            tracing::error!("save_artifact failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// A safe artifact name is a single path segment: no separators, no `.`
/// traversal, no NUL, bounded length (Stage 2.2).
fn is_safe_artifact_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return false;
    }
    if name == "." || name == ".." || name.starts_with("../") || name.starts_with("..\\") {
        return false;
    }
    name.chars().all(|c| !c.is_control())
}

/// Resolve the SSE `after` cursor for a reconnect: `Last-Event-ID` header
/// (browser default) seeds it, but an explicit query `after_sequence` wins.
/// Either way the next poll starts after the last delivered sequence, so a
/// reconnect reads no gaps and no duplicates.
fn sse_resume_after(after_sequence: u64, last_event_id: Option<&axum::http::HeaderValue>) -> u64 {
    let last = last_event_id
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    match last {
        Some(last) => after_sequence.max(last),
        None => after_sequence,
    }
}

async fn events_stream(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
    Query(q): Query<EventsQuery>,
    headers: HeaderMap,
) -> axum::response::sse::Sse<
    impl Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::{Event, Sse};
    use std::time::Duration;
    // SSE reconnect resume: `Last-Event-ID` header (browser default on
    // reconnect) seeds `after`, but an explicit `after_sequence` query wins
    // (lets a client force a different point). This gives no gaps/no dups:
    // the next poll starts after the last delivered sequence.
    let mut after = sse_resume_after(q.after_sequence, headers.get("Last-Event-ID"));
    let stream = async_stream::stream! {
        loop {
            match state.store.get_events(&task_id, after).await {
                Ok(events) if !events.is_empty() => {
                    for e in events {
                        after = after.max(e.sequence);
                        if let Ok(data) = serde_json::to_string(&e) {
                            // Set the SSE `id:` field to the sequence so a
                            // browser will send `Last-Event-ID` on reconnect;
                            // the `after_sequence` query is the explicit path.
                            yield Ok(
                                Event::default()
                                    .event("task-event")
                                    .id(e.sequence.to_string())
                                    .data(data),
                            );
                        }
                    }
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

async fn get_events(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Json<Vec<agentgrid_common::TaskEvent>> {
    match state.store.get_events(&task_id, q.after_sequence).await {
        Ok(e) => Json(e),
        Err(e) => {
            tracing::error!("get_events failed: {e}");
            Json(vec![])
        }
    }
}

async fn poll(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Json(mut req): Json<PollRequest>,
) -> (StatusCode, Json<PollResponse>) {
    // The authenticated node id is the source of truth; ignore any client-supplied id.
    req.node_id = auth.node_id;
    if agentgrid_common::is_incompatible_protocol(&req.protocol_version) {
        let _ = state.store.set_node_degraded(&req.node_id).await;
    }
    if let Err(e) = state.store.register_or_touch_node(&req).await {
        tracing::error!("register node failed: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(PollResponse { assignment: None }),
        );
    }

    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        match state.store.try_assign(&req.node_id).await {
            Ok(Some(assignment)) => {
                return (
                    StatusCode::OK,
                    Json(PollResponse {
                        assignment: Some(assignment),
                    }),
                );
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!("try_assign failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(PollResponse { assignment: None }),
                );
            }
        }
        if Instant::now() >= deadline {
            return (StatusCode::OK, Json(PollResponse { assignment: None }));
        }
        let remaining = deadline - Instant::now();
        tokio::select! {
            _ = state.assignment_notify.notified() => {}
            _ = tokio::time::sleep(remaining) => {
                return (StatusCode::OK, Json(PollResponse { assignment: None }));
            }
        }
    }
}

async fn attempt_cancel_handler(
    State(state): State<Arc<AppState>>,
    Path(attempt_id): Path<String>,
) -> Json<CancelState> {
    let requested = state
        .store
        .attempt_cancel_requested(&attempt_id)
        .await
        .unwrap_or(false);
    Json(CancelState {
        cancel_requested: requested,
    })
}

async fn cancel_task_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Path(task_id): Path<String>,
) -> StatusCode {
    match state.store.cancel_task(&task_id).await {
        Ok(true) => {
            let _ = state
                .store
                .audit(
                    "user",
                    auth.as_ref().map(|e| e.0.username.as_str()),
                    "task.cancel",
                    Some(&task_id),
                    None,
                )
                .await;
            StatusCode::OK
        }
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("cancel_task failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[derive(Debug, Deserialize)]
struct ApprovalListQuery {
    status: Option<String>,
}

async fn list_approvals_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ApprovalListQuery>,
) -> Result<Json<Vec<ApprovalView>>, StatusCode> {
    let status = q
        .status
        .and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok());
    state
        .store
        .list_approvals(status)
        .await
        .map(Json)
        .map_err(|e| {
            tracing::error!("list_approvals failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn allow_approval_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Path(id): Path<String>,
    body: Option<Json<AnswerApprovalBody>>,
) -> StatusCode {
    let actor = auth
        .as_ref()
        .map(|e| e.0.username.as_str())
        .unwrap_or("system");
    let reason = body.and_then(|b| b.0.reason).filter(|s| !s.is_empty());
    match state
        .store
        .answer_approval(&id, ApprovalEvent::Allow, reason.as_deref(), actor)
        .await
    {
        Ok(_) => StatusCode::OK,
        Err(e) => {
            tracing::error!("allow_approval failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn deny_approval_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Path(id): Path<String>,
    body: Option<Json<AnswerApprovalBody>>,
) -> StatusCode {
    let actor = auth
        .as_ref()
        .map(|e| e.0.username.as_str())
        .unwrap_or("system");
    let reason = body
        .and_then(|b| b.0.reason)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "denied by operator".to_string());
    match state
        .store
        .answer_approval(&id, ApprovalEvent::Deny, Some(&reason), actor)
        .await
    {
        Ok(_) => StatusCode::OK,
        Err(e) => {
            tracing::error!("deny_approval failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[derive(Debug, Deserialize)]
struct AnswerApprovalBody {
    /// Optional operator reason recorded with the decision (shown in the UI/CLI
    /// and audit). Omitted = default placeholder.
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateApprovalBody {
    attempt_id: String,
    session_id: Option<String>,
    permission: serde_json::Value,
    #[serde(default)]
    scope: Option<String>,
}

/// Stage 5: an ACP agent's `session/request_permission` creates a durable,
/// operator-answerable approval. Returns its id so the daemon can poll.
async fn create_approval_for_task_handler(
    State(state): State<Arc<AppState>>,
    _auth: Option<Extension<AuthedUser>>,
    Path(task_id): Path<String>,
    Json(body): Json<CreateApprovalBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let perm = serde_json::to_string(&body.permission).unwrap_or_default();
    match state
        .store
        .create_approval(
            &task_id,
            &body.attempt_id,
            body.session_id.as_deref(),
            &perm,
            300,
            None,
            body.scope.as_deref().unwrap_or("session"),
        )
        .await
    {
        Ok(id) => Ok(Json(serde_json::json!({ "id": id }))),
        Err(e) => {
            tracing::error!("create_approval failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_approval_handler(
    State(state): State<Arc<AppState>>,
    _auth: Option<Extension<AuthedUser>>,
    Path(id): Path<String>,
) -> Result<Json<ApprovalView>, StatusCode> {
    match state.store.get_approval(&id).await {
        Ok(Some(v)) => Ok(Json(v)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("get_approval failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn retry_task_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Path(task_id): Path<String>,
) -> StatusCode {
    match state.store.retry_task(&task_id).await {
        Ok(true) => {
            let _ = state
                .store
                .audit(
                    "user",
                    auth.as_ref().map(|e| e.0.username.as_str()),
                    "task.retry",
                    Some(&task_id),
                    None,
                )
                .await;
            StatusCode::OK
        }
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("retry_task failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn ingest_events(
    State(state): State<Arc<AppState>>,
    Path(attempt_id): Path<String>,
    Json(req): Json<IngestEventsRequest>,
) -> StatusCode {
    for e in &req.events {
        if e.payload.to_string().len() > state.limits.event {
            return StatusCode::PAYLOAD_TOO_LARGE;
        }
    }
    match state.store.ingest_events(&attempt_id, &req).await {
        Ok(true) => StatusCode::OK,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("ingest_events failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn complete_attempt(
    State(state): State<Arc<AppState>>,
    Path(attempt_id): Path<String>,
    Json(req): Json<CompleteAttemptRequest>,
) -> StatusCode {
    match state.store.complete_attempt(&attempt_id, &req).await {
        Ok(true) => StatusCode::OK,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("complete_attempt failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn ack_attempt_handler(
    State(state): State<Arc<AppState>>,
    Path(attempt_id): Path<String>,
) -> StatusCode {
    match state.store.ack_attempt(&attempt_id).await {
        Ok(true) => StatusCode::OK,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("ack_attempt failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn create_agent_session_handler(
    State(state): State<Arc<AppState>>,
    Path(attempt_id): Path<String>,
    Json(req): Json<CreateAgentSessionRequest>,
) -> Response {
    match state
        .store
        .create_agent_session(&attempt_id, &req.adapter)
        .await
    {
        Ok(id) => (
            StatusCode::OK,
            Json(serde_json::json!({ "session_id": id })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("create_agent_session failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// Bind and serve. Starts background maintenance (lease/heartbeat jobs).
/// If `AGENTGRID_TLS_CERT` and `AGENTGRID_TLS_KEY` are both set, the listener is
/// wrapped in a rustls TLS acceptor (no system OpenSSL); otherwise plaintext.
pub async fn serve(state: Arc<AppState>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    if let Err(e) = state.store.reconcile_on_startup().await {
        tracing::warn!("startup reconcile failed: {e}");
    }
    state.store.start_maintenance();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let app = build_router(state.clone());
    match (
        std::env::var("AGENTGRID_TLS_CERT"),
        std::env::var("AGENTGRID_TLS_KEY"),
    ) {
        (Ok(cert), Ok(key)) => {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let acceptor = load_tls_acceptor(&cert, &key)?;
            tracing::info!("control plane listening with TLS on {addr}");
            axum::serve(
                TlsListener {
                    tcp: listener,
                    acceptor,
                },
                app,
            )
            .with_graceful_shutdown(shutdown_signal(state.clone()))
            .await?;
        }
        _ => {
            tracing::info!("control plane listening on {addr} (plaintext)");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal(state.clone()))
                .await?;
        }
    }
    Ok(())
}

/// TLS-wrapped listener implementing axum 0.8's `Listener` trait, so it drops
/// straight into `axum::serve`. Performs the TLS handshake per accepted TCP
/// stream; a failed handshake is logged and the accept loop continues.
struct TlsListener {
    tcp: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.tcp.accept().await {
                Ok((stream, addr)) => match self.acceptor.accept(stream).await {
                    Ok(tls) => return (tls, addr),
                    Err(e) => tracing::warn!("tls handshake failed: {e}"),
                },
                Err(e) => {
                    tracing::error!("accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// Build a rustls acceptor from a PEM cert chain + private key (no system OpenSSL).
fn load_tls_acceptor(cert_path: &str, key_path: &str) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    let cert_pem =
        std::fs::read(cert_path).with_context(|| format!("read TLS cert {cert_path}"))?;
    let key_pem = std::fs::read(key_path).with_context(|| format!("read TLS key {key_path}"))?;
    let mut cert_reader = std::io::Cursor::new(&cert_pem[..]);
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader).collect::<Result<_, _>>()?;
    let mut key_reader = std::io::Cursor::new(&key_pem[..]);
    let key = rustls_pemfile::private_key(&mut key_reader)?
        .context("no private key found in TLS key PEM")?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build rustls server config")?;
    Ok(tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config)))
}

/// Await Ctrl-C / SIGTERM, then truncate the WAL so a restart replays nothing
/// stale (Stage 2.5 ops).
async fn shutdown_signal(state: Arc<AppState>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                let _ = sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    let _ = state.store.wal_checkpoint().await;
}

#[cfg(test)]
mod tls_tests {
    use super::*;

    #[test]
    fn load_tls_acceptor_missing_file_errors() {
        assert!(load_tls_acceptor("/no/such/cert.pem", "/no/such/key.pem").is_err());
    }
}

#[cfg(test)]
mod sse_tests {
    use super::sse_resume_after;

    fn header(v: &str) -> axum::http::HeaderValue {
        axum::http::HeaderValue::from_str(v).unwrap()
    }

    #[test]
    fn resume_uses_query_when_higher_than_header() {
        // Explicit query wins over Last-Event-ID when query is newer.
        assert_eq!(sse_resume_after(5, Some(&header("2"))), 5);
    }

    #[test]
    fn resume_uses_header_when_higher_than_query() {
        // Last-Event-ID promotes a reconnect that started at 0 up to last seq.
        assert_eq!(sse_resume_after(0, Some(&header("7"))), 7);
    }

    #[test]
    fn resume_takes_max_of_both() {
        assert_eq!(sse_resume_after(3, Some(&header("3"))), 3);
    }

    #[test]
    fn resume_without_header_is_query() {
        assert_eq!(sse_resume_after(9, None), 9);
    }

    #[test]
    fn resume_ignores_non_numeric_header() {
        // A garbage Last-Event-ID falls back to the query (no gaps, no dup).
        assert_eq!(sse_resume_after(4, Some(&header("garbage"))), 4);
    }
}
