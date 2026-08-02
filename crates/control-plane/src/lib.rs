//! Control plane for agentgrid.
//!
//! HTTP surface (`/v1`) and long-poll scheduler are stable; the backing store
//! is SQLite (see [`store`]). Stage 1 used an in-memory map — swapped for
//! persistence in Stage 2.1.

pub mod store;
pub mod workflow;

use crate::store::is_safe_opaque_id;
use anyhow::Context;
use std::sync::Arc;
use std::time::Instant;

use agentgrid_common::{
    AppendMessageRequest, ApprovalEvent, ApprovalView, CancelState, CompleteAttemptRequest,
    CreateAgentSessionRequest, CreateConversationRequest, CreateRepositoryRequest,
    CreateTaskRequest, CreateWorkflowRequest, CreateWorkflowRunRequest, EnrollRequest,
    EnrollResponse, EnrollTokenResponse, EventsQuery, HeartbeatRequest, IngestEventsRequest,
    LoginRequest, LoginResponse, McpServer, McpServerCreate, PollRequest, PollResponse,
    RepositoryView, SetupRequest, TaskEligibility, TaskView, UploadArtifactRequest,
    WorkflowProjection, WorkflowRun, WorkflowRunWithSteps, WorkflowSchedule,
    WorkflowScheduleCreate, WorkflowTemplate,
};
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Extension, Path, Query, State},
    http::{header, HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use futures_core::Stream;
use http_body_util::BodyExt;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use store::Store;
use tokio::sync::Notify;
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// JWT claims for user sessions (Stage 4.1).
/// Includes `jti` (JWT ID) for session revocation (Stage 4.2).
#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    exp: usize,
    jti: String,
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
    /// Hardening P1 item 14: per-node event-ingest rate limit (requests per
    /// window). A node that floods the control plane with event batches beyond
    /// `event_rate_max` / `event_rate_window_secs` gets 429 instead of more
    /// DB writes. Defaults: 60 req / 10s (covers a healthy streamer).
    event_rate: Arc<tokio::sync::Mutex<EventRate>>,
    /// One-time bootstrap setup token (hardening P0): printed once to stdout
    /// when no users exist; required to create the first user; consumed on
    /// first use. `None` once bootstrap is complete or the token has been used
    /// / has expired (15 min TTL).
    setup_token: Arc<tokio::sync::Mutex<Option<SetupToken>>>,
    /// Hardening P2 item 35: security observability counters, surfaced in
    /// /metrics so an operator can alert on rising cross-node rejections, stale
    /// fencing tokens, or event-batch rejection.
    pub cross_node_rejects: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub stale_fencing_tokens: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub event_rejections: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Hardening P1 item 14: cumulative count of event-sequence gaps a batch
    /// introduced (a sequence > current contiguous-prefix+1). Out-of-order /
    /// skipped-sequence redelivery bumps this monotonically.
    pub event_gaps: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// Request size ceilings (Stage 5.1). Overridable via env; defaults:
/// prompt 64 KiB, event payload 1 MiB, artifact 50 MiB.
struct Limits {
    prompt: usize,
    event: usize,
    artifact: usize,
    /// Hardening P1 (event ingestion): cap events per batch and the total
    /// batch payload size, so a node cannot flood the control plane with one
    /// giant request or O(events) inserts in a single transaction.
    event_batch_count: usize,
    event_batch_bytes: usize,
}

/// One-time bootstrap setup token (hardening P0). Printed to stdout once on
/// first start; must be presented to `POST /v1/auth/setup` to create the
/// first user; consumed on first use; expires after `SETUP_TOKEN_TTL`.
struct SetupToken {
    token: String,
    issued_at: std::time::Instant,
}

const SETUP_TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

impl SetupToken {
    fn new() -> Self {
        use rand::Rng;
        // 32 hex chars from a random u128; sufficient for a short-lived,
        // one-time bootstrap token printed to stdout.
        let token = format!("{:032x}", rand::thread_rng().gen::<u128>());
        Self {
            token,
            issued_at: std::time::Instant::now(),
        }
    }

    /// True if the token has not expired.
    fn is_live(&self) -> bool {
        self.issued_at.elapsed() < SETUP_TOKEN_TTL
    }
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

/// Hardening P1 item 14: per-node event-ingest rate limiter. Each node has its
/// own fixed window counter, pruned lazily when next touched past the window.
/// Defaults are tuned via `AGENTGRID_EVENT_RATE_MAX` (req / window) and
/// `AGENTGRID_EVENT_RATE_WINDOW_SECS`.
struct EventRate {
    per_node: std::collections::HashMap<String, (i64, u32)>,
    max: u32,
    window_secs: i64,
}

impl EventRate {
    fn new() -> Self {
        Self {
            per_node: std::collections::HashMap::new(),
            max: env_usize("AGENTGRID_EVENT_RATE_MAX", 60) as u32,
            window_secs: env_usize("AGENTGRID_EVENT_RATE_WINDOW_SECS", 10) as i64,
        }
    }

    /// `true` if this request is under the per-node budget; the first request of
    /// a new window resets the counter.
    fn admit(&mut self, node_id: &str, now: i64) -> bool {
        let entry = self.per_node.entry(node_id.to_string()).or_insert((now, 0));
        if now - entry.0 >= self.window_secs {
            entry.0 = now;
            entry.1 = 0;
        }
        entry.1 += 1;
        entry.1 <= self.max
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
        let production = std::env::var("AGENTGRID_ENV").as_deref() == Ok("production");
        let jwt_secret = match std::env::var("AGENTGRID_JWT_SECRET") {
            Ok(s) => {
                // Hardening P0: a weak JWT secret forges session cookies.
                if s.len() < 32 {
                    if production {
                        anyhow::bail!(
                            "AGENTGRID_JWT_SECRET is shorter than 32 bytes; \
                             refusing to start in AGENTGRID_ENV=production"
                        );
                    }
                    tracing::warn!(
                        "AGENTGRID_JWT_SECRET is shorter than 32 bytes; \
                         session tokens are vulnerable to brute force"
                    );
                }
                s.into_bytes()
            }
            Err(_) => {
                // A random-per-start secret invalidates previously issued
                // *user session* JWTs after a restart (node credentials are
                // independent: they are hashed and stored in `nodes`, not
                // signed with this secret). In production a stable secret is
                // mandatory; elsewhere we warn and use a random one.
                if production {
                    anyhow::bail!(
                        "AGENTGRID_JWT_SECRET unset; refusing to start in \
                         AGENTGRID_ENV=production (set a stable >=32-byte secret)"
                    );
                }
                tracing::warn!(
                    "AGENTGRID_JWT_SECRET unset: using a random secret for this run; \
                     existing user session JWTs will not survive a restart"
                );
                use rand::Rng;
                rand::thread_rng().gen::<[u8; 32]>().to_vec()
            }
        };
        // Hardening P0: when no users exist (fresh install), mint a
        // one-time setup token printed to stdout so only an operator with
        // console access can create the first admin. Consumed on first use;
        // expires after SETUP_TOKEN_TTL. Env bootstrap removed (backdoor).
        let setup_token = Arc::new(tokio::sync::Mutex::new(if store.user_count().await? == 0 {
            let t = SetupToken::new();
            println!(
                "\n=== agentgrid setup token (one-time, expires in {} min) ===\n{}\n=== \
                     present this at POST /v1/auth/setup to create the first user ===",
                SETUP_TOKEN_TTL.as_secs() / 60,
                t.token
            );
            Some(t)
        } else {
            None
        }));
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
        // Hardening P0 item 4: canonicalize the web root once at startup so the
        // static fallback can compare against a fixed canonical root on every
        // request (symlinks escaping the source dir are caught at request time
        // by re-canonicalizing the served file). Fails closed if the resolved
        // root does not exist by startup time.
        let web_root = match web_root {
            Some(r) => match r.canonicalize() {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(
                        root = %r.display(),
                        "web root failed to canonicalize at startup, static UI disabled: {e}"
                    );
                    None
                }
            },
            None => None,
        };
        let limits = Limits {
            prompt: env_usize("AGENTGRID_MAX_PROMPT_KB", 64) * 1024,
            event: env_usize("AGENTGRID_MAX_EVENT_KB", 1024) * 1024,
            artifact: env_usize("AGENTGRID_MAX_ARTIFACT_MB", 50) * 1024 * 1024,
            event_batch_count: env_usize("AGENTGRID_MAX_EVENT_BATCH", 500),
            event_batch_bytes: env_usize("AGENTGRID_MAX_EVENT_BATCH_KB", 4096) * 1024,
        };
        Ok(Arc::new(Self {
            store,
            assignment_notify: Arc::new(Notify::new()),
            jwt_secret,
            web_root,
            limits,
            db_path: db_path.to_string(),
            login_rate: Arc::new(tokio::sync::Mutex::new(LoginRate::new())),
            event_rate: Arc::new(tokio::sync::Mutex::new(EventRate::new())),
            setup_token,
            cross_node_rejects: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            stale_fencing_tokens: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            event_rejections: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            event_gaps: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }))
    }

    /// Open a fresh temporary database with no users (used by tests that
    /// exercise the bootstrap/setup flow). A one-time setup token is minted
    /// and printed to stdout (same as a fresh install).
    pub async fn open_temp_fresh() -> anyhow::Result<Arc<Self>> {
        let p = std::env::temp_dir().join(format!("ag-test-{}.db", Uuid::new_v4()));
        Self::open(p.to_str().unwrap()).await
    }

    /// Open a fresh temporary database (used by tests). Bootstraps a
    /// `test`/`test` user so the closed bootstrap window does not block
    /// test task creation; tests then login to obtain a JWT.
    pub async fn open_temp() -> anyhow::Result<Arc<Self>> {
        let p = std::env::temp_dir().join(format!("ag-test-{}.db", Uuid::new_v4()));
        let state = Self::open(p.to_str().unwrap()).await?;
        if state.store.user_count().await? == 0 {
            state.store.create_user("test", "test").await?;
        }
        Ok(state)
    }

    /// Issue a 12h JWT for `username` (Stage 4.1).
    /// Includes `jti` for session revocation (Stage 4.2).
    fn issue_token(&self, username: &str) -> anyhow::Result<String> {
        let exp = (chrono::Utc::now() + chrono::Duration::hours(12)).timestamp() as usize;
        let jti = Uuid::new_v4().to_string();
        let claims = Claims {
            sub: username.to_string(),
            exp,
            jti,
        };
        Ok(encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(&self.jwt_secret),
        )?)
    }

    /// Validate a JWT and return the username, or None if revoked/invalid.
    /// Checks revoked_sessions blocklist (Stage 4.2).
    async fn verify_token(&self, token: &str) -> Option<String> {
        let claims = decode::<Claims>(
            token,
            &DecodingKey::from_secret(&self.jwt_secret),
            &Validation::default(),
        )
        .ok()?;
        // Check if this jti has been revoked
        if self
            .store
            .is_session_revoked(&claims.claims.jti)
            .await
            .ok()?
        {
            return None;
        }
        Some(claims.claims.sub)
    }

    /// Read the current one-time setup token (if live) for tests / operators
    /// who missed the stdout print. Does not consume it; `auth_setup` consumes
    /// on first successful use.
    pub async fn setup_token(&self) -> Option<String> {
        let guard = self.setup_token.lock().await;
        guard
            .as_ref()
            .filter(|t| t.is_live())
            .map(|t| t.token.clone())
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
        .route(
            "/v1/mcp-servers",
            get(list_mcp_servers_handler).post(create_mcp_server_handler),
        )
        .route("/v1/mcp-servers/{id}", delete(delete_mcp_server_handler))
        .route("/v1/profiles", get(list_profiles_handler))
        .route("/v1/profiles/{id}", get(get_profile_handler))
        .route("/v1/profiles/{id}", post(create_profile_handler))
        .route("/v1/profiles/{id}/activate", post(activate_profile_handler))
        .route("/v1/admin/backup", post(admin_backup))
        .route("/v1/admin/storage-gc", post(storage_gc_handler))
        .route("/v1/nodes", get(list_nodes))
        .route("/v1/nodes/enrollment-token", post(create_enrollment_token))
        .route("/v1/nodes/{id}", delete(revoke_node))
        .route("/v1/nodes/{id}/drain", post(drain_node_handler))
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
            "/v1/node/attempts/{id}/begin_validate",
            post(begin_validate_handler),
        )
        .route(
            "/v1/node/attempts/{id}/session",
            post(create_agent_session_handler),
        )
        .route("/v1/node/attempts/{id}/artifacts", post(upload_artifact))
        .route(
            "/v1/node/attempts/{id}/artifacts/raw",
            post(upload_artifact_raw),
        )
        .route("/v1/tasks/{id}/artifacts/{name}", get(get_artifact))
        .route(
            "/v1/node/tasks/{id}/artifacts/{name}",
            get(get_artifact_node),
        )
        .route("/v1/conversations", post(create_conversation))
        .route("/v1/conversations/{id}", get(show_conversation))
        .route(
            "/v1/conversations/{id}/messages",
            post(append_conversation_message).get(list_conversation_messages),
        )
        .route("/v1/workflows", post(create_workflow).get(list_workflows))
        .route("/v1/workflows/{id}", get(show_workflow))
        .route("/v1/workflows/{id}/runs", post(create_workflow_run))
        .route(
            "/v1/workflows/{id}/schedules",
            post(create_workflow_schedule).get(list_workflow_schedules),
        )
        .route(
            "/v1/workflows/{id}/schedules/{sid}",
            delete(delete_workflow_schedule),
        )
        .route("/v1/workflow-runs", get(list_workflow_runs))
        .route("/v1/workflow-runs/{id}", get(show_workflow_run))
        .route(
            "/v1/workflow-runs/{id}/projection",
            get(workflow_run_projection),
        )
        .route("/v1/workflow-runs/{id}/tick", post(tick_workflow_run))
        .route("/v1/workflow-runs/{id}/plan", get(workflow_run_plan))
        .route(
            "/v1/workflow-runs/{id}/approve-plan",
            post(approve_workflow_plan_handler),
        )
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
        // Hardening P2 item 36: default security headers on every response
        // (Referrer-Policy + a restrictive Permissions-Policy). HSTS is opt-in
        // via AGENTGRID_HSTS=1 so a plain-HTTP-control-plane-terminated TLS at
        // a reverse proxy does not pin a self-signed cert. The per-route CSP
        // (set on the SPA shell + artifact responses) stays untouched.
        .layer(middleware::from_fn(security_headers_middleware))
        // Hardening P2 item 19/35: outermost layer so every log line inside a
        // request (auth, handler, store) carries a stable `request_id`. The
        // JSON formatter attaches the current span to each event, so the id is
        // correlatable across the whole request without per-handler plumbing.
        .layer(middleware::from_fn(request_id_middleware))
        .fallback(spa_fallback)
        .with_state(state)
}

/// Hardening P2 item 19/35: assign a request id per request. Accept a client
/// `X-Request-Id` only if it is a safe opaque id (≤64 `[A-Za-z0-9_-]`, no
/// separators — same guard as attempt ids, so a client cannot inject log
/// control chars or forge another request's id); otherwise mint a UUIDv4.
/// Hardening P2 item 36: apply a small set of default security headers to
/// every response. Referrer-Policy + a restrictive Permissions-Policy
/// (no camera/mic/geolocation/USB/etc.) keep the API/UI safe even when a
/// browser somehow loads a response context. HSTS is opt-in behind
/// AGENTGRID_HSTS=1 so a loopback / reverse-proxied TLS control plane does
/// not pin the wrong cert; the per-route CSP (SPA shell + artifacts) is set
/// downstream and is left untouched here.
async fn security_headers_middleware(req: Request<Body>, next: Next) -> Response {
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    let _ = headers.try_insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    let _ = headers.try_insert(
        header::HeaderName::from_static("permissions-policy"),
        header::HeaderValue::from_static(
            "accelerometer=(), camera=(), geolocation=(), gyroscope=(), magnetometer=(), microphone=(), payment=(), usb=()",
        ),
    );
    if std::env::var("AGENTGRID_HSTS").as_deref() == Ok("1") {
        let _ = headers.try_insert(
            header::HeaderName::from_static("strict-transport-security"),
            header::HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        );
    }
    res
}

/// The id rides in a tracing span (so every event logs it) and is echoed back
/// on the response.
async fn request_id_middleware(headers: HeaderMap, mut req: Request<Body>, next: Next) -> Response {
    let id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| is_safe_opaque_id(s))
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    req.extensions_mut().insert(RequestId(id.clone()));
    let span = tracing::info_span!("request", request_id = %id);
    let mut resp = span.in_scope(|| async move { next.run(req).await }).await;
    resp.headers_mut()
        .insert("x-request-id", id.as_str().try_into().unwrap());
    resp
}

/// Hardening P2 item 19: a single, machine-readable JSON error envelope used
/// by handlers that already surface a typed status. `code` is a stable
/// snake_case string clients can switch on; the human message is short and
/// never includes internal error chains (those stay in structured logs).
/// The `X-Request-Id` header (added by the middleware) stays the correlation
/// key; it is also embedded in the body for clients that only read the body.
pub fn api_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let req_id = uuid::Uuid::new_v4().to_string();
    let body = serde_json::json!({
        "error": {
            "code": code,
            "message": message.into(),
            "request_id": req_id,
        }
    });
    (status, Json(body)).into_response()
}

/// Request id available to handlers via extensions (for explicit logging /
/// future audit). The span already carries it for every log line.
#[derive(Clone, Debug)]
pub struct RequestId(pub String);

/// Serve the built web UI (Stage 4.3). Unknown non-API paths fall back to
/// SPA static file serving using tower-http's ServeDir.
/// Serves files from the web root with proper security headers.
/// Falls back to index.html for non-/v1/ routes (SPA routing).
async fn spa_fallback(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let web_root = match &state.web_root {
        Some(r) => r.clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let path = req.uri().path();
    // Don't serve SPA fallback for /v1/ API routes - return 404
    if path.starts_with("/v1/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Hardening P0: validate path components to prevent traversal.
    // Only Normal segments allowed; ParentDir (..), RootDir (/), and
    // any prefix components are rejected.
    let rel = path.trim_start_matches('/');
    // Inline is_safe_static_path check
    let is_safe = {
        use std::path::Component;
        let mut safe = true;
        for comp in std::path::Path::new(rel).components() {
            match comp {
                Component::Normal(_) => {}
                Component::CurDir => {}
                _ => {
                    safe = false;
                    break;
                }
            }
        }
        safe
    };
    if !is_safe {
        return StatusCode::FORBIDDEN.into_response();
    }
    // Hardening P0: reject symlinks that escape the web root.
    // Check each path component for symlinks pointing outside root.
    let fs_path = web_root.join(rel);
    if let Ok(canon_file) = fs_path.canonicalize() {
        if !canon_file.starts_with(web_root.canonicalize().unwrap_or_default()) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    // Serve index.html for root path explicitly
    if rel.is_empty() {
        let idx = web_root.join("index.html");
        if let Ok(bytes) = tokio::fs::read(&idx).await {
            return (
                [
                    (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    (axum::http::header::CACHE_CONTROL, "no-cache"),
                    (axum::http::header::HeaderName::from_static("content-security-policy"),
                        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline';                          img-src 'self' data:; connect-src 'self'; font-src 'self';                          object-src 'none'; base-uri 'self'; frame-ancestors 'none';                          form-action 'self'"),
                    (axum::http::header::HeaderName::from_static("x-content-type-options"), "nosniff"),
                    (axum::http::header::HeaderName::from_static("x-frame-options"), "DENY"),
                ],
                bytes,
            ).into_response();
        }
        return StatusCode::NOT_FOUND.into_response();
    }
    let serve_dir = ServeDir::new(&web_root)
        .not_found_service(ServeFile::new(web_root.join("index.html")))
        .append_index_html_on_directories(false);
    // Convert ServeDir response to axum Response<Body>
    match tower::util::ServiceExt::oneshot(serve_dir, req).await {
        Ok(res) => {
            let (parts, body) = res.into_parts();
            Response::from_parts(parts, Body::new(body.into_stream()))
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Check if a relative path is safe for static file serving.
/// Only Normal path components are allowed; ParentDir (..), RootDir (/),
/// and any prefix components are rejected.
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

/// Reject a node request whose attempt is not owned by the authenticated
/// node. Hardening P0: cross-node isolation. Returns `Ok(())` if the attempt
/// exists and belongs to `auth.node_id`; `Err(FORBIDDEN)` if it exists but
/// belongs to another node; `Err(NOT_FOUND)` if no such attempt (avoid
/// disclosing existence). Optionally allow a revoked caller to also hit 403 —
/// but revoked nodes already fail at `require_node_auth` (no credential match).
async fn check_attempt_owner(
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
async fn check_fencing_token(
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
        // migrated CP) has a blank token; accept any presenter to avoid
        // breaking in-flight nodes before they roll the token.
        Ok(Some(s)) if s.is_empty() => Ok(()),
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
fn fencing_token_header(h: &HeaderMap) -> Option<String> {
    h.get("x-agentgrid-fencing-token")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
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

/// Require a valid user JWT on user-facing routes. Hardening P0:
/// - DB error in `user_count` fails closed (503), never opens the API.
/// - Before the first user exists (bootstrap not complete), all `/v1/` user
///   routes are closed (503) except `/v1/auth/setup`; static UI (non-`/v1/`
///   paths) stays served so the setup page loads. Node routes are handled
///   by [`require_node_auth`] and are skipped here.
async fn require_user_auth(
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

async fn auth_logout(
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

async fn workflow_run_plan(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<agentgrid_common::WorkflowProjection>, StatusCode> {
    // Stage 13: exposes the pending plan on a `PlanReady` run (the projection
    // already carries `run.status`); the bare plan text is the architect's
    // emitted YAML/JSON — read-only so an operator can inspect before approving.
    let _ = state.store.get_workflow_run_plan(&id).await;
    match state.store.get_workflow_run_projection(&id).await {
        Ok(Some(p)) => Ok(Json(p)),
        // 404 if the run doesn't exist; the plan field lives on the projection.
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

async fn approve_workflow_plan_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<agentgrid_common::WorkflowRunWithSteps>, StatusCode> {
    match state.store.approve_workflow_plan(&id).await {
        Ok(()) => {
            // Wake the scheduler so the freshly-expanded steps assign.
            state.assignment_notify.notify_waiters();
            match state.store.get_workflow_run(&id).await {
                Ok(Some(r)) => {
                    let steps = state
                        .store
                        .get_workflow_run_steps(&id)
                        .await
                        .unwrap_or_default();
                    Ok(Json(agentgrid_common::WorkflowRunWithSteps {
                        run: r,
                        steps,
                    }))
                }
                Ok(None) => Err(StatusCode::NOT_FOUND),
                Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
            }
        }
        Err(e) => {
            tracing::warn!("approve_workflow_plan failed for {id}: {e}");
            // Wrong-state / bad-plan => 409, missing run => 404, other => 500.
            let msg = e.to_string();
            if msg.contains("unknown workflow run") || msg.contains("no plan to approve") {
                Err(StatusCode::NOT_FOUND)
            } else if msg.contains("is not awaiting plan approval") || msg.contains("plan") {
                Err(StatusCode::CONFLICT)
            } else {
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
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

/// Hardening P1 item 15: `ag storage gc` — reconcile the artifact tree against
/// the metadata table. `dry_run=true` only reports drift
/// `{orphan_files, orphan_bytes, metadata_without_file, free_mb}`; `false`
/// deletes orphan files and prunes dangling metadata rows.
#[derive(serde::Deserialize)]
struct StorageGcRequest {
    #[serde(default)]
    dry_run: bool,
}

async fn storage_gc_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StorageGcRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    match state.store.storage_reconcile(req.dry_run).await {
        Ok((orphan_files, orphan_bytes, metadata_without_file)) => {
            let free = state.store.free_bytes() / (1024 * 1024);
            let _ = state
                .store
                .audit(
                    "system",
                    None,
                    "storage.gc",
                    None,
                    Some(
                        &serde_json::json!({
                            "dry_run": req.dry_run,
                            "orphan_files": orphan_files,
                            "orphan_bytes": orphan_bytes,
                            "metadata_without_file": metadata_without_file,
                        })
                        .to_string(),
                    ),
                )
                .await;
            Ok(Json(serde_json::json!({
                "dry_run": req.dry_run,
                "orphan_files": orphan_files,
                "orphan_bytes": orphan_bytes,
                "metadata_without_file": metadata_without_file,
                "free_mb": free,
            })))
        }
        Err(e) => {
            tracing::error!("storage gc failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
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

/// Stage 13: list all registered MCP servers.
async fn list_mcp_servers_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<McpServer>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — surface
    // storage outage as 503 so the client does not read it as "no servers".
    match state.store.list_mcp_servers().await {
        Ok(s) => Ok(Json(s)),
        Err(e) => {
            tracing::error!("list_mcp_servers failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// Stage 13: register (or replace) an MCP server in the operator registry.
async fn create_mcp_server_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<McpServerCreate>,
) -> Result<Json<McpServer>, StatusCode> {
    state
        .store
        .upsert_mcp_server(&req)
        .await
        .map(Json)
        .map_err(|e| {
            tracing::warn!("create_mcp_server failed: {e}");
            StatusCode::BAD_REQUEST
        })
}

/// Stage 13: delete an MCP server from the registry.
async fn delete_mcp_server_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> StatusCode {
    match state.store.delete_mcp_server(&id).await {
        Ok(true) => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("delete_mcp_server failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
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

    // Hardening P2 item 35: security-observability counters.
    s.push_str(
        "# HELP agentgrid_cross_node_rejects_total Cross-node mutation/read attempts rejected (wrong owner).
",
    );
    s.push_str("# TYPE agentgrid_cross_node_rejects_total counter\n");
    s.push_str(&format!(
        "agentgrid_cross_node_rejects_total {}\n",
        state
            .cross_node_rejects
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(
        "# HELP agentgrid_stale_fencing_tokens_total Mutations rejected for a stale fencing token.
",
    );
    s.push_str("# TYPE agentgrid_stale_fencing_tokens_total counter\n");
    s.push_str(&format!(
        "agentgrid_stale_fencing_tokens_total {}\n",
        state
            .stale_fencing_tokens
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(
        "# HELP agentgrid_event_rejections_total Event batches rejected (terminal attempt / too large / count cap).
",
    );
    s.push_str("# TYPE agentgrid_event_rejections_total counter\n");
    s.push_str(&format!(
        "agentgrid_event_rejections_total {}\n",
        state
            .event_rejections
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    // Hardening P1 item 14: event-sequence gaps a batch introduced (max
    // sequence in the batch exceeded the contiguous prefix). Monotonic across
    // batches; the durable outbox still redrives the missing sequences.
    s.push_str(
        "# HELP agentgrid_event_gaps_total Event-sequence gaps introduced by a batch (max seq > contiguous prefix).",
    );
    s.push_str("\n# TYPE agentgrid_event_gaps_total counter\n");
    s.push_str(&format!(
        "agentgrid_event_gaps_total {}\n",
        state.event_gaps.load(std::sync::atomic::Ordering::Relaxed)
    ));
    // Hardening P2 item 35: lease-expiry reverts (the lease/ACK race path).
    s.push_str(
        "# HELP agentgrid_lease_reverts_total Expired-lease assignments re-queued by the sweep.",
    );
    s.push_str("\n# TYPE agentgrid_lease_reverts_total counter\n");
    s.push_str(&format!(
        "agentgrid_lease_reverts_total {}\n",
        state
            .store
            .lease_reverts
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(
        "# HELP agentgrid_active_attempt_drift_total Drifted active_attempts counters repaired by reconcile.",
    );
    s.push_str("\n# TYPE agentgrid_active_attempt_drift_total counter\n");
    s.push_str(&format!(
        "agentgrid_active_attempt_drift_total {}\n",
        state
            .store
            .active_attempt_drift
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(
        "# HELP agentgrid_artifact_cleanup_bytes_total Cumulative bytes reclaimed by artifact retention.",
    );
    s.push_str("\n# TYPE agentgrid_artifact_cleanup_bytes_total counter\n");
    s.push_str(&format!(
        "agentgrid_artifact_cleanup_bytes_total {}\n",
        state
            .store
            .artifact_cleanup_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str("# HELP agentgrid_artifact_cleanup_runs_total Total artifact cleanup runs.");
    s.push_str("\n# TYPE agentgrid_artifact_cleanup_runs_total counter\n");
    s.push_str(&format!(
        "agentgrid_artifact_cleanup_runs_total {}\n",
        state
            .store
            .artifact_cleanup_runs
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str("# HELP agentgrid_artifact_cleanup_failures_total Total artifact cleanup failures.");
    s.push_str("\n# TYPE agentgrid_artifact_cleanup_failures_total counter\n");
    s.push_str(&format!(
        "agentgrid_artifact_cleanup_failures_total {}\n",
        state
            .store
            .artifact_cleanup_failures
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(
        "# HELP agentgrid_artifact_cleanup_duration_seconds_total Total artifact cleanup duration in seconds.",
    );
    s.push_str("\n# TYPE agentgrid_artifact_cleanup_duration_seconds_total counter\n");
    s.push_str(&format!(
        "agentgrid_artifact_cleanup_duration_seconds_total {}\n",
        state
            .store
            .artifact_cleanup_duration_secs
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

async fn list_tasks(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TaskListQuery>,
) -> Result<Json<Vec<TaskView>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — that
    // would read as "no tasks" to the client. Surface storage outage as 503.
    match state
        .store
        .list_tasks_filtered(
            q.status.as_deref(),
            q.repository.as_deref(),
            q.node_id.as_deref(),
            task_cursor(&q),
            q.limit,
        )
        .await
    {
        Ok(t) => Ok(Json(t)),
        Err(e) => {
            tracing::error!("list_tasks failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// Hardening P2 item 20: optional server-side filters + keyset cursor for
/// `GET /v1/tasks`. `after_created_at` + `after_id` form a keyset cursor
/// (rows strictly after `(created_at, id)`); `limit` caps the page (server
/// ceiling 1000). Both cursor parts must be present together.
#[derive(Debug, Default, serde::Deserialize)]
struct TaskListQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    after_created_at: Option<String>,
    #[serde(default)]
    after_id: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

/// Combine the optional keyset-cursor parts into the store's `Option<(String,
/// String)>`. Only a complete pair is a cursor; a lone half is ignored so old
/// clients (and garbage input) fall back to the first page.
fn task_cursor(q: &TaskListQuery) -> Option<(String, String)> {
    match (&q.after_created_at, &q.after_id) {
        (Some(c), Some(i)) if !c.is_empty() && !i.is_empty() => Some((c.clone(), i.clone())),
        _ => None,
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
            budget: t.budget,
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
        budget: req.budget.clone(),
        created_at: String::new(),
    }
    .validate_dag()
    .map_err(|e| {
        tracing::warn!("workflow DAG invalid: {e}");
        StatusCode::BAD_REQUEST
    })?;
    state
        .store
        .create_workflow_template(&req.name, &req.steps, &req.budget)
        .await
        .map(|t| (StatusCode::CREATED, Json(t)))
        .map_err(|e| {
            tracing::error!("create_workflow failed: {e}");
            StatusCode::BAD_REQUEST
        })
}

async fn list_workflows(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<WorkflowTemplate>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — surface
    // storage outage as 503 so the client does not read it as "no workflows".
    match state.store.list_workflow_templates().await {
        Ok(t) => Ok(Json(t)),
        Err(e) => {
            tracing::error!("list_workflows failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
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

/// Stage 13: create a scheduled trigger for a workflow template.
async fn create_workflow_schedule(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<WorkflowScheduleCreate>,
) -> Result<(StatusCode, Json<WorkflowSchedule>), StatusCode> {
    state
        .store
        .create_workflow_schedule(&id, &req)
        .await
        .map(|s| (StatusCode::CREATED, Json(s)))
        .map_err(|e| {
            tracing::warn!("create_workflow_schedule failed: {e}");
            StatusCode::BAD_REQUEST
        })
}

async fn list_workflow_schedules(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<WorkflowSchedule>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — surface
    // storage outage as 503 so the client does not read it as "no schedules".
    match state.store.list_workflow_schedules(Some(&id)).await {
        Ok(s) => Ok(Json(s)),
        Err(e) => {
            tracing::error!("list_workflow_schedules failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

async fn delete_workflow_schedule(
    State(state): State<Arc<AppState>>,
    Path((_id, sid)): Path<(String, String)>,
) -> StatusCode {
    match state.store.delete_workflow_schedule(&sid).await {
        Ok(true) => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("delete_workflow_schedule failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn list_workflow_runs(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<WorkflowRun>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — surface
    // storage outage as 503 so the client does not read it as "no runs".
    match state.store.list_workflow_runs().await {
        Ok(r) => Ok(Json(r)),
        Err(e) => {
            tracing::error!("list_workflow_runs failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
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

async fn list_nodes(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<agentgrid_common::NodeView>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — that
    // would read as "no nodes" to the client. Surface storage outage as 503.
    match state.store.list_nodes().await {
        Ok(n) => Ok(Json(n)),
        Err(e) => {
            tracing::error!("list_nodes failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
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
        Ok(true) => {
            // Stage 9.2: auto-fill the trust ledger from heartbeat discovery.
            // Upsert is idempotent and never overwrites an operator decision.
            let discovered: Vec<(String, String)> = req
                .discovered_skills
                .iter()
                .map(|s| (s.name.clone(), s.source.clone()))
                .collect();
            if !discovered.is_empty() {
                if let Err(e) = state.store.upsert_discovered_skills(&discovered).await {
                    // Discovery is best-effort; never fail the heartbeat on it.
                    tracing::warn!("skill discovery upsert failed: {e}");
                }
            }
            StatusCode::OK
        }
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("heartbeat failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn revoke_node(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Path(id): Path<String>,
) -> StatusCode {
    match state.store.revoke_node(&id).await {
        Ok(true) => {
            // Hardening P2 item 35: audit the security-sensitive mutation.
            let _ = state
                .store
                .audit(
                    "user",
                    auth.as_ref().map(|e| e.0.username.as_str()),
                    "node.revoke",
                    Some(&id),
                    None,
                )
                .await;
            StatusCode::OK
        }
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("revoke_node failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Hardening P2 item 37: drain (or undrain) a node for maintenance. A drained
/// node stops receiving NEW assignments while its in-flight attempts run to
/// completion; the heartbeat keeps it online.
async fn drain_node_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Path(id): Path<String>,
    Query(q): Query<NodeDrainQuery>,
) -> StatusCode {
    let drained = q.drain.unwrap_or(true);
    match state.store.set_node_drained(&id, drained).await {
        Ok(true) => {
            let _ = state
                .store
                .audit(
                    "user",
                    auth.as_ref().map(|e| e.0.username.as_str()),
                    if drained {
                        "node.drain"
                    } else {
                        "node.undrain"
                    },
                    Some(&id),
                    None,
                )
                .await;
            StatusCode::OK
        }
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("set_node_drained failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[derive(serde::Deserialize)]
struct NodeDrainQuery {
    #[serde(default)]
    drain: Option<bool>,
}

async fn create_repository(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Json(req): Json<CreateRepositoryRequest>,
) -> Result<(StatusCode, Json<RepositoryView>), (StatusCode, Json<serde_json::Value>)> {
    // Hardening P1 item 32: vet the git_url scheme at the trust boundary so an
    // operator cannot register a `javascript:`/`data:`/arbitrary URI git remote.
    // Allow only git transports; `file://` is permitted for trusted local clones
    // (test repos) — narrowing `file://` to a policy flag is a P1 follow-up.
    if let Err(msg) = validate_git_url(&req.git_url) {
        tracing::warn!("create_repository rejected git_url: {msg}");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": msg})),
        ));
    }
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
            Ok((StatusCode::CREATED, Json(v)))
        }
        Err(e) => {
            tracing::error!("create_repository failed: {e}");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal error"})),
            ))
        }
    }
}

/// Hardening P1 item 32: accept only git-class URL schemes. Returns Err(msg)
/// for a scheme that could never be a legitimate git remote or that carries
/// injection risk (`javascript:`, `data:`, ...). `scp`-style (`user@host:path`)
/// and bare relative paths are allowed as git does.
fn validate_git_url(url: &str) -> Result<(), &'static str> {
    if url.is_empty() || url.trim().is_empty() {
        return Err("git_url must not be empty");
    }
    if url.contains('\n') || url.contains('\r') {
        return Err("git_url must not contain newlines");
    }
    if let Some(idx) = url.find(':') {
        if url[idx..].starts_with("://") {
            if !matches!(&url[..idx], "http" | "https" | "git" | "ssh" | "file") {
                return Err("git_url scheme not allowed");
            }
        } else if !url[..idx].contains('@') {
            // single colon, no `@host` -> a `scheme:path` URI (javascript:/data:),
            // which git never accepts. scp-style `user@host:path` is fine.
            return Err("git_url scheme not allowed");
        }
    }
    Ok(())
}

async fn list_repositories(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<RepositoryView>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — surface
    // storage outage as 503 so the client does not read it as "no repos".
    match state.store.list_repositories().await {
        Ok(r) => Ok(Json(r)),
        Err(e) => {
            tracing::error!("list_repositories failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
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
) -> Result<Response, StatusCode> {
    // Stage 2.2: a crafted name (../, absolute, ...) must not traverse out of
    // the artifact root via store::read_artifact's join. Reject as 404 so a
    // denial does not disclose whether the task/artifact exists.
    if !is_safe_artifact_name(&name) {
        return Err(StatusCode::NOT_FOUND);
    }
    match state.store.read_artifact_bytes(&task_id, &name).await {
        Ok(Some(bytes)) => {
            let mt = state
                .store
                .read_artifact_meta(&task_id, &name)
                .await
                .ok()
                .flatten();
            Ok(artifact_response(
                bytes,
                mt.as_ref().and_then(|m| m.media_type.as_deref()),
                &name,
                mt.as_ref().and_then(|m| m.sha256.as_deref()),
            ))
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("read_artifact failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_artifact_node(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path((task_id, name)): Path<(String, String)>,
) -> Result<Response, StatusCode> {
    // Stage 8 / line 257: node-side mirror of `get_artifact` so the node can
    // fetch an upstream worker's `changes.patch` artifact with its own
    // node-credential (no user JWT available on the node). Same safety:
    // reject traversal names as 404.
    if !is_safe_artifact_name(&name) {
        return Err(StatusCode::NOT_FOUND);
    }
    // Hardening P0: authorize the caller node to read this producer task's
    // artifact (workflow dependency producer -> consumer attempt owned by
    // the caller). Always 404 on denial to avoid disclosing existence.
    let allowed = state
        .store
        .can_node_read_upstream_artifact(&auth.node_id, &task_id)
        .await
        .map_err(|e| {
            tracing::error!("can_node_read_upstream_artifact failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if !allowed {
        return Err(StatusCode::NOT_FOUND);
    }
    match state.store.read_artifact_bytes(&task_id, &name).await {
        Ok(Some(bytes)) => {
            let mt = state
                .store
                .read_artifact_meta(&task_id, &name)
                .await
                .ok()
                .flatten();
            Ok(artifact_response(
                bytes,
                mt.as_ref().and_then(|m| m.media_type.as_deref()),
                &name,
                mt.as_ref().and_then(|m| m.sha256.as_deref()),
            ))
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("read_artifact (node) failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn upload_artifact(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<UploadArtifactRequest>,
) -> Response {
    // Stage 2.2: never let a crafted name escape the artifact root
    // (../../etc/passwd, absolute paths, separators). Validated before the
    // ownership check so a traversal attempt never reaches the store and
    // never discloses attempt existence.
    if !is_safe_artifact_name(&req.name) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Err(code) = check_attempt_owner(&state, &auth, &attempt_id).await {
        return code.into_response();
    }
    if let Err(code) = check_fencing_token(
        &state,
        &attempt_id,
        fencing_token_header(&headers).as_deref(),
    )
    .await
    {
        return code.into_response();
    }
    if req.content.len() > state.limits.artifact {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    match state
        .store
        .save_artifact_bytes(
            &attempt_id,
            &req.name,
            req.content.as_bytes(),
            req.media_type.as_deref(),
            req.sha256.as_deref(),
        )
        .await
    {
        Ok(resp) => axum::Json(resp).into_response(),
        Err(crate::store::StoreArtifactError::HashMismatch { .. }) => {
            StatusCode::UNPROCESSABLE_ENTITY.into_response()
        }
        Err(crate::store::StoreArtifactError::InvalidAttemptId) => {
            StatusCode::BAD_REQUEST.into_response()
        }
        Err(e) => {
            tracing::error!("save_artifact failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Stage 2.2 binary-safe artifact upload: the request body is raw bytes (not
/// UTF-8 JSON), with the artifact name, optional media type, and optional hex
/// SHA-256 carried in headers. Idempotent per (attempt_id, name) on the store.
/// The node uses this for `changes.patch` (binary diffs) and any non-text
/// artifact; the legacy JSON endpoint stays for text-only clients.
async fn upload_artifact_raw(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(code) = check_attempt_owner(&state, &auth, &attempt_id).await {
        return code.into_response();
    }
    if let Err(code) = check_fencing_token(
        &state,
        &attempt_id,
        fencing_token_header(&headers).as_deref(),
    )
    .await
    {
        return code.into_response();
    }
    if body.len() > state.limits.artifact {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let name = match headers
        .get("x-artifact-name")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
    {
        Some(n) => n,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };
    if !is_safe_artifact_name(&name) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let media_type = headers
        .get("x-artifact-media-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let sha256 = headers
        .get("x-artifact-sha256")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    match state
        .store
        .save_artifact_bytes(
            &attempt_id,
            &name,
            &body,
            media_type.as_deref(),
            sha256.as_deref(),
        )
        .await
    {
        Ok(resp) => axum::Json(resp).into_response(),
        Err(crate::store::StoreArtifactError::HashMismatch { .. }) => {
            StatusCode::UNPROCESSABLE_ENTITY.into_response()
        }
        Err(crate::store::StoreArtifactError::InvalidAttemptId) => {
            StatusCode::BAD_REQUEST.into_response()
        }
        Err(e) => {
            tracing::error!("save_artifact_bytes failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Hardening P0 (stored XSS / download safety): build a `Response` for a
/// served artifact. A small allowlist of inline-safe media types may be
/// served with their stored `Content-Type`; everything else is forced to
/// `application/octet-stream`. HTML / SVG / JavaScript / XML and unknown
/// types are always sent as an **attachment** with `Content-Disposition`,
/// and every artifact response adds `X-Content-Type-Options: nosniff` so a
/// browser never sniffs a download as HTML/script. `name` is encoded as a
/// safe RFC 6266 `filename` (ASCII-only, control chars stripped).
fn artifact_response(
    bytes: Vec<u8>,
    media_type: Option<&str>,
    name: &str,
    sha256: Option<&str>,
) -> Response {
    const INLINE_SAFE: &[&str] = &[
        "application/octet-stream",
        "text/plain",
        "application/json",
        "application/zip",
        "application/gzip",
        "application/x-tar",
        "application/x-bzip2",
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/webp",
    ];
    const ACTIVE: &[&str] = &[
        "text/html",
        "text/xml",
        "application/xml",
        "application/xhtml+xml",
        "image/svg+xml",
        "application/javascript",
        "text/javascript",
        "application/ecmascript",
    ];
    let stored = media_type.unwrap_or("application/octet-stream").trim();
    let (content_type, attachment) = if ACTIVE.contains(&stored) {
        ("application/octet-stream", true)
    } else if INLINE_SAFE.contains(&stored) {
        (stored, false)
    } else {
        // Unknown type: never trust the client-requested type inline.
        ("application/octet-stream", true)
    };
    // ponytail: extension-based sniﬃng is a P2 follow-up; the allowlist +
    // nosniff + attachment triplet already blocks inline exﬆecution.
    let safe_name: String = name
        .chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control() && *c != '/' && *c != '\\' && *c != '"')
        .collect::<String>()
        .trim_start_matches('.')
        .to_string();
    let mut resp = (
        [
            (header::CONTENT_TYPE, content_type),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            ),
            // Hardening P0 item 3: artifacts are opaque data, never a document
            // context. `default-src 'none'` blocks any plugin/inline execution
            // even if a browser ignores nosniff; CORP same-origin keeps a
            // cross-origin page from reading artifact bytes.
            (
                header::HeaderName::from_static("content-security-policy"),
                "default-src 'none'; frame-ancestors 'none'",
            ),
            (
                header::HeaderName::from_static("cross-origin-resource-policy"),
                "same-origin",
            ),
        ],
        Bytes::from(bytes),
    )
        .into_response();
    if attachment {
        let cd = if safe_name.is_empty() {
            "attachment".to_string()
        } else {
            format!("attachment; filename=\"{}\"", safe_name)
        };
        if let Ok(v) = axum::http::HeaderValue::from_str(&cd) {
            resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
        }
    }
    // Hardening P2 item 36: expose the server-computed content hash so
    // clients (web UI) can show the artifact's integrity digest.
    if let Some(sha) = sha256 {
        if let Ok(v) = axum::http::HeaderValue::from_str(sha) {
            resp.headers_mut()
                .insert(header::HeaderName::from_static("x-artifact-sha256"), v);
        }
    }
    resp
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

/// Resolve the SSE `after` cursor for a reconnect. `Last-Event-ID` header
/// (browser default) carries the global `ingest_id` of the last delivered
/// event (Hardening P0 item 9), and an explicit `after_ingest` query wins.
/// Legacy `after_sequence` (per-attempt) is still accepted when neither is
/// set, so pre-0037 clients keep working.
fn sse_resume_after(
    after_ingest: Option<u64>,
    after_sequence: u64,
    last_event_id: Option<&axum::http::HeaderValue>,
) -> (Option<u64>, u64) {
    let last = last_event_id
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    match last {
        // `Last-Event-ID` is an ingest_id (the id: field on events since 0037).
        Some(last) => (Some(last.max(after_ingest.unwrap_or(0))), after_sequence),
        None => (after_ingest, after_sequence),
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
    // reconnect) carries the global ingest_id of the last delivered event, but
    // an explicit `after_ingest` query wins (lets a client force a different
    // point). This gives no gaps/no dups across attempts: the next poll starts
    // after the last delivered ingest_id.
    let (mut after_ingest, after_sequence) = sse_resume_after(
        q.after_ingest,
        q.after_sequence,
        headers.get("Last-Event-ID"),
    );
    let stream = async_stream::stream! {
        loop {
            match state
                .store
                .get_events(&task_id, after_ingest, after_sequence, Some(500))
                .await
            {
                Ok(events) if !events.is_empty() => {
                    for e in events {
                        // Track both cursors: the global ingest_id drives
                        // pagination; the legacy sequence stays accurate for
                        // old clients that keep polling after_sequence.
                        after_ingest = Some(after_ingest.unwrap_or(0).max(e.ingest_id));
                        if let Ok(data) = serde_json::to_string(&e) {
                            // SSE `id:` is the global ingest_id so a browser
                            // sends `Last-Event-ID` as an ingest cursor on
                            // reconnect (Hardening P0 item 9).
                            yield Ok(
                                Event::default()
                                    .event("task-event")
                                    .id(e.ingest_id.to_string())
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
) -> Result<Json<Vec<agentgrid_common::TaskEvent>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — surface
    // storage outage as 503 with a machine-readable code so the client does
    // not read it as "no events".
    match state
        .store
        .get_events(&task_id, q.after_ingest, q.after_sequence, q.limit)
        .await
    {
        Ok(e) => Ok(Json(e)),
        Err(e) => {
            tracing::error!("get_events failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
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
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
) -> Response {
    if let Err(code) = check_attempt_owner(&state, &auth, &attempt_id).await {
        return code.into_response();
    }
    // No fencing check here: this is a read of cancel_requested (a polling
    // endpoint), not a mutation.
    let requested = state
        .store
        .attempt_cancel_requested(&attempt_id)
        .await
        .unwrap_or(false);
    Json(CancelState {
        cancel_requested: requested,
    })
    .into_response()
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
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<IngestEventsRequest>,
) -> Response {
    if let Err(code) = check_attempt_owner(&state, &auth, &attempt_id).await {
        return code.into_response();
    }
    if let Err(code) = check_fencing_token(
        &state,
        &attempt_id,
        fencing_token_header(&headers).as_deref(),
    )
    .await
    {
        return code.into_response();
    }
    for e in &req.events {
        if e.payload.to_string().len() > state.limits.event {
            state
                .event_rejections
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
    }
    // Hardening P1 item 14: per-node rate limit so a single node cannot flood
    // the control plane with event batches. Over the limit → 429, with a
    // counted rejection so it shows in metrics.
    {
        let now = chrono::Utc::now().timestamp();
        let allowed = state.event_rate.lock().await.admit(&auth.node_id, now);
        if !allowed {
            state
                .event_rejections
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    }
    // Hardening P1 item 14: bound the batch itself — event count and the
    // summed payload bytes — so one request cannot flood the store.
    if req.events.len() > state.limits.event_batch_count {
        state
            .event_rejections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let total: usize = req.events.iter().map(|e| e.payload.to_string().len()).sum();
    if total > state.limits.event_batch_bytes {
        state
            .event_rejections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    match state.store.ingest_events(&attempt_id, &req).await {
        Ok(ack) if ack.accepted > 0 || ack.highest_contiguous_sequence.is_some() => {
            // Hardening P1 item 14: a batch whose max sequence exceeds the
            // contiguous prefix (highest_contiguous_sequence) introduced a
            // gap — out-of-order or skipped-sequence delivery. Count it once
            // per such batch for observability; the durable outbox still
            // redrives the missing sequences.
            let max_seq = req.events.iter().map(|e| e.sequence).max();
            if let (Some(max_seq), Some(prefix)) = (max_seq, ack.highest_contiguous_sequence) {
                if max_seq > prefix {
                    state
                        .event_gaps
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            (StatusCode::OK, Json(ack)).into_response()
        }
        // Either the attempt is gone or it is terminal — both are
        // rejections worth surfacing in observability.
        Ok(_) => {
            state
                .event_rejections
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            StatusCode::NOT_FOUND.into_response()
        }
        Err(e) => {
            tracing::error!("ingest_events failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn complete_attempt(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<CompleteAttemptRequest>,
) -> Response {
    if let Err(code) = check_attempt_owner(&state, &auth, &attempt_id).await {
        return code.into_response();
    }
    if let Err(code) = check_fencing_token(
        &state,
        &attempt_id,
        fencing_token_header(&headers).as_deref(),
    )
    .await
    {
        return code.into_response();
    }
    match state.store.complete_attempt(&attempt_id, &req).await {
        Ok(true) => StatusCode::OK.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            // Hardening P1 item 13: map invalid state transition to 409 Conflict
            // with a machine-readable code (hardening P2 item 19). The `?`
            // operator wraps the raw `InvalidTransition` in anyhow (via its
            // blanket From), NOT the StoreTransitionError marker — so check
            // both shapes.
            if e.downcast_ref::<crate::store::StoreTransitionError>()
                .is_some()
                || e.downcast_ref::<agentgrid_common::InvalidTransition>()
                    .is_some()
            {
                return api_error(
                    StatusCode::CONFLICT,
                    "invalid_state_transition",
                    "attempt cannot transition from its current state",
                );
            }
            tracing::error!("complete_attempt failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn ack_attempt_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
    headers: HeaderMap,
) -> StatusCode {
    if let Err(code) = check_attempt_owner(&state, &auth, &attempt_id).await {
        return code;
    }
    if let Err(code) = check_fencing_token(
        &state,
        &attempt_id,
        fencing_token_header(&headers).as_deref(),
    )
    .await
    {
        return code;
    }
    match state.store.ack_attempt(&attempt_id).await {
        Ok(true) => StatusCode::OK,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("ack_attempt failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Hardening P0 item 12: the node signals it has started the post-agent
/// validation command; the CP moves the attempt (and task) `running →
/// validating`. Ownership + fencing checked like every other node mutation.
async fn begin_validate_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
    headers: HeaderMap,
) -> StatusCode {
    if let Err(code) = check_attempt_owner(&state, &auth, &attempt_id).await {
        return code;
    }
    if let Err(code) = check_fencing_token(
        &state,
        &attempt_id,
        fencing_token_header(&headers).as_deref(),
    )
    .await
    {
        return code;
    }
    match state.store.begin_validate(&attempt_id).await {
        Ok(true) => StatusCode::OK,
        // Not `running` (already validating, terminal, or gone): idempotent
        // no-op so a retried validation signal never 500s.
        Ok(false) => StatusCode::OK,
        Err(e) => {
            tracing::error!("begin_validate failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn create_agent_session_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<CreateAgentSessionRequest>,
) -> Response {
    if let Err(code) = check_attempt_owner(&state, &auth, &attempt_id).await {
        return code.into_response();
    }
    if let Err(code) = check_fencing_token(
        &state,
        &attempt_id,
        fencing_token_header(&headers).as_deref(),
    )
    .await
    {
        return code.into_response();
    }
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
            // Hardening P2 item 19: log the full internal error chain here, but
            // never send it to the client — return an opaque 500 body.
            tracing::error!("create_agent_session failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal error"})),
            )
                .into_response()
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
    // Stage 13 / line 487: re-advance in-flight workflow runs so a CP restart
    // does not strand them; idempotent, best-effort per run.
    state.store.start_workflow_ticker();
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
        // Explicit after_ingest query wins over Last-Event-ID when newer.
        assert_eq!(
            sse_resume_after(Some(5), 0, Some(&header("2"))),
            (Some(5), 0)
        );
    }

    #[test]
    fn resume_uses_header_when_higher_than_query() {
        // Last-Event-ID promotes a reconnect that started at 0 up to last
        // ingest_id (Hardening P0 item 9: the header carries the global cursor).
        assert_eq!(sse_resume_after(None, 0, Some(&header("7"))), (Some(7), 0));
    }

    #[test]
    fn resume_takes_max_of_both() {
        assert_eq!(
            sse_resume_after(Some(3), 0, Some(&header("3"))),
            (Some(3), 0)
        );
    }

    #[test]
    fn resume_without_header_is_query() {
        assert_eq!(sse_resume_after(Some(9), 0, None), (Some(9), 0));
    }

    #[test]
    fn resume_ignores_non_numeric_header() {
        // A garbage Last-Event-ID falls back to the query (no gaps, no dup).
        assert_eq!(
            sse_resume_after(None, 0, Some(&header("garbage"))),
            (None, 0)
        );
    }

    #[test]
    fn resume_legacy_sequence_without_ingest_cursor() {
        // Pre-0037 client: no after_ingest, no Last-Event-ID — the per-attempt
        // after_sequence is passed through unchanged.
        assert_eq!(sse_resume_after(None, 42, None), (None, 42));
    }
}

#[cfg(test)]
mod artifact_response_tests {
    use super::artifact_response;
    use axum::body::{to_bytes, Body};
    use axum::http::Response;

    fn hdr(resp: &Response<Body>, name: &str) -> Option<String> {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }

    #[tokio::test]
    async fn html_served_as_attachment_octetstream_with_nosniff() {
        let resp = artifact_response(b"<html></html>".to_vec(), Some("text/html"), "x.html", None);
        assert_eq!(
            hdr(&resp, "content-type").as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(
            hdr(&resp, "x-content-type-options").as_deref(),
            Some("nosniff")
        );
        let cd = hdr(&resp, "content-disposition").unwrap_or_default();
        assert!(cd.starts_with("attachment"), "cd={cd}");
        assert!(cd.contains("filename=\"x.html\""));
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    }

    #[tokio::test]
    async fn svg_served_as_attachment_octetstream_with_nosniff() {
        let resp = artifact_response(b"<svg/>".to_vec(), Some("image/svg+xml"), "logo.svg", None);
        assert_eq!(
            hdr(&resp, "content-type").as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(
            hdr(&resp, "x-content-type-options").as_deref(),
            Some("nosniff")
        );
        let cd = hdr(&resp, "content-disposition").unwrap_or_default();
        assert!(cd.starts_with("attachment") && cd.contains("filename=\"logo.svg\""));
    }

    #[tokio::test]
    async fn png_allowed_inline_no_attachment() {
        let resp = artifact_response(b"\x89PNG".to_vec(), Some("image/png"), "blob.png", None);
        assert_eq!(hdr(&resp, "content-type").as_deref(), Some("image/png"));
        assert_eq!(
            hdr(&resp, "x-content-type-options").as_deref(),
            Some("nosniff")
        );
        assert!(
            hdr(&resp, "content-disposition").is_none(),
            "no attachment for inline-safe"
        );
    }

    #[tokio::test]
    async fn unknown_type_forced_octetstream_attachment() {
        let resp = artifact_response(
            b"x".to_vec(),
            Some("application/x-crazy"),
            "weird.dat",
            None,
        );
        assert_eq!(
            hdr(&resp, "content-type").as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(
            hdr(&resp, "x-content-type-options").as_deref(),
            Some("nosniff")
        );
        assert!(hdr(&resp, "content-disposition")
            .unwrap_or_default()
            .contains("attachment"));
    }

    #[tokio::test]
    async fn filename_control_and_separator_chars_stripped() {
        // name "../e<TAB>v<QUOTE>x" -> separators/control/quote stripped to "evx"
        let resp = artifact_response(b"x".to_vec(), Some("text/html"), "../e\tv\"x", None);
        let cd = hdr(&resp, "content-disposition").unwrap_or_default();
        assert!(
            cd.contains("filename=\"evx\"") || cd == "attachment",
            "cd={cd}"
        );
    }
}
