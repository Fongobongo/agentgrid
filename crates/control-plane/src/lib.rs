//! Control plane for agentgrid.
//!
//! HTTP surface (`/v1`) and long-poll scheduler are stable; the backing store
//! is SQLite (see [`store`]). Stage 1 used an in-memory map — swapped for
//! persistence in Stage 2.1.

mod auth;
use auth::{AuthedUser, Claims};
mod config;
mod middleware;
mod routes;
pub mod store;
mod tls;
pub mod workflow;

// OpenTelemetry metrics (optional feature)
pub mod otel;

use crate::config::{env_usize, EventRate, Limits, LoginRate, SetupToken, SETUP_TOKEN_TTL};
use crate::store::is_safe_opaque_id;
use std::sync::Arc;

use agentgrid_common::{McpServer, McpServerCreate};
use axum::{
    extract::{DefaultBodyLimit, Extension, Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use store::Store;
use tokio::sync::Notify;
use uuid::Uuid;

pub(crate) const POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// Stage 2.5: the cookie name carrying the session JWT, set HttpOnly so the
/// browser cannot read it (no XSS token theft) with SameSite=Strict (CSRF
/// guard). `Secure` is added only when `AGENTGRID_COOKIE_SECURE=1` so local
/// plaintext dev keeps working.
pub struct AppState {
    pub store: Store,
    pub(crate) assignment_notify: Arc<Notify>,
    pub(crate) jwt_secret: Vec<u8>,
    /// Directory with the built web UI (Stage 4.3). Served as static files;
    /// `None` disables the UI.
    pub(crate) web_root: Option<std::path::PathBuf>,
    /// Request size ceilings (Stage 5.1).
    pub(crate) limits: Limits,
    /// Database file path (for SQLite size metrics, Stage 5.2).
    db_path: String,
    /// Brute-force protection on `/v1/auth/login` (Stage 2.5).
    pub(crate) login_rate: Arc<tokio::sync::Mutex<LoginRate>>,
    /// Hardening P1 item 14: per-node event-ingest rate limit (requests per
    /// window). A node that floods the control plane with event batches beyond
    /// `event_rate_max` / `event_rate_window_secs` gets 429 instead of more
    /// DB writes. Defaults: 60 req / 10s (covers a healthy streamer).
    pub(crate) event_rate: Arc<tokio::sync::Mutex<EventRate>>,
    /// One-time bootstrap setup token (hardening P0): printed once to stdout
    /// when no users exist; required to create the first user; consumed on
    /// first use. `None` once bootstrap is complete or the token has been used
    /// / has expired (15 min TTL).
    pub(crate) setup_token: Arc<tokio::sync::Mutex<Option<SetupToken>>>,
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
        let p = std::path::Path::new("/var/tmp").join(format!("ag-test-{}.db", Uuid::new_v4()));
        Self::open(p.to_str().unwrap()).await
    }

    /// Open a fresh temporary database (used by tests). Bootstraps a
    /// `test`/`test` user so the closed bootstrap window does not block
    /// test task creation; tests then login to obtain a JWT.
    pub async fn open_temp() -> anyhow::Result<Arc<Self>> {
        let p = std::path::Path::new("/var/tmp").join(format!("ag-test-{}.db", Uuid::new_v4()));
        let state = Self::open(p.to_str().unwrap()).await?;
        if state.store.user_count().await? == 0 {
            state.store.create_user("test", "test").await?;
        }
        Ok(state)
    }

    /// Issue a 12h JWT for `username` (Stage 4.1).
    /// Includes `jti` for session revocation (Stage 4.2).
    pub(crate) fn issue_token(&self, username: &str) -> anyhow::Result<String> {
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
    pub(crate) async fn verify_token(&self, token: &str) -> Option<String> {
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
        .route("/health/live", get(auth::health_live))
        .route("/health/ready", get(auth::health_ready))
        .route("/metrics", get(metrics))
        .route(
            "/v1/tasks",
            post(routes::tasks::create_task).get(routes::tasks::list_tasks),
        )
        .route("/v1/tasks/{id}", get(routes::tasks::show_task))
        .route("/v1/tasks/{id}/events", get(routes::events::get_events))
        .route(
            "/v1/tasks/{id}/events/stream",
            get(routes::events::events_stream),
        )
        .route(
            "/v1/tasks/{id}/cancel",
            post(routes::tasks::cancel_task_handler),
        )
        .route(
            "/v1/tasks/{id}/retry",
            post(routes::tasks::retry_task_handler),
        )
        .route(
            "/v1/tasks/{id}/eligibility",
            get(routes::tasks::task_eligibility_handler),
        )
        .route(
            "/v1/approvals",
            get(routes::approvals::list_approvals_handler),
        )
        .route(
            "/v1/approvals/{id}",
            get(routes::approvals::get_approval_handler),
        )
        .route(
            "/v1/approvals/{id}/allow",
            post(routes::approvals::allow_approval_handler),
        )
        .route(
            "/v1/approvals/{id}/deny",
            post(routes::approvals::deny_approval_handler),
        )
        .route(
            "/v1/tasks/{id}/approvals",
            post(routes::approvals::create_approval_for_task_handler),
        )
        .route("/v1/auth/setup", post(auth::auth_setup))
        .route("/v1/auth/login", post(auth::auth_login))
        .route("/v1/auth/logout", post(auth::auth_logout))
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
        .route("/v1/nodes", get(routes::nodes::list_nodes))
        .route(
            "/v1/nodes/enrollment-token",
            post(routes::nodes::create_enrollment_token),
        )
        .route("/v1/nodes/{id}", delete(routes::nodes::revoke_node))
        .route(
            "/v1/nodes/{id}/drain",
            post(routes::nodes::drain_node_handler),
        )
        .route(
            "/v1/repositories",
            post(routes::repositories::create_repository)
                .get(routes::repositories::list_repositories),
        )
        .route("/v1/node/enroll", post(routes::nodes::enroll))
        .route("/v1/node/poll", post(routes::events::poll))
        .route("/v1/node/heartbeat", post(routes::nodes::heartbeat))
        .route(
            "/v1/node/attempts/{id}/cancel",
            get(routes::attempts::attempt_cancel_handler),
        )
        .route(
            "/v1/node/attempts/{id}/events",
            post(routes::attempts::ingest_events),
        )
        .route(
            "/v1/node/attempts/{id}/complete",
            post(routes::attempts::complete_attempt),
        )
        .route(
            "/v1/node/attempts/{id}/ack",
            post(routes::attempts::ack_attempt_handler),
        )
        .route(
            "/v1/node/attempts/{id}/begin_validate",
            post(routes::attempts::begin_validate_handler),
        )
        .route(
            "/v1/node/attempts/{id}/session",
            post(routes::attempts::create_agent_session_handler),
        )
        .route(
            "/v1/node/attempts/{id}/artifacts",
            post(routes::artifacts::upload_artifact),
        )
        .route(
            "/v1/node/attempts/{id}/artifacts/raw",
            post(routes::artifacts::upload_artifact_raw),
        )
        .route(
            "/v1/tasks/{id}/artifacts/{name}",
            get(routes::artifacts::get_artifact),
        )
        .route(
            "/v1/node/tasks/{id}/artifacts/{name}",
            get(routes::artifacts::get_artifact_node),
        )
        .route(
            "/v1/conversations",
            post(routes::conversations::create_conversation),
        )
        .route(
            "/v1/conversations/{id}",
            get(routes::conversations::show_conversation),
        )
        .route(
            "/v1/conversations/{id}/messages",
            post(routes::conversations::append_conversation_message)
                .get(routes::conversations::list_conversation_messages),
        )
        .route(
            "/v1/workflows",
            post(routes::workflows::create_workflow).get(routes::workflows::list_workflows),
        )
        .route("/v1/workflows/{id}", get(routes::workflows::show_workflow))
        .route(
            "/v1/workflows/{id}/runs",
            post(routes::workflows::create_workflow_run),
        )
        .route(
            "/v1/workflows/{id}/schedules",
            post(routes::workflows::create_workflow_schedule)
                .get(routes::workflows::list_workflow_schedules),
        )
        .route(
            "/v1/workflows/{id}/schedules/{sid}",
            delete(routes::workflows::delete_workflow_schedule),
        )
        .route(
            "/v1/workflow-runs",
            get(routes::workflows::list_workflow_runs),
        )
        .route(
            "/v1/workflow-runs/{id}",
            get(routes::workflows::show_workflow_run),
        )
        .route(
            "/v1/workflow-runs/{id}/projection",
            get(routes::workflows::workflow_run_projection),
        )
        .route(
            "/v1/workflow-runs/{id}/tick",
            post(routes::workflows::tick_workflow_run),
        )
        .route(
            "/v1/workflow-runs/{id}/plan",
            get(routes::workflows::workflow_run_plan),
        )
        .route(
            "/v1/workflow-runs/{id}/approve-plan",
            post(routes::workflows::approve_workflow_plan_handler),
        )
        .route(
            "/v1/workflow-runs/{id}/cancel",
            post(routes::workflows::cancel_workflow_run_handler),
        )
        .layer(DefaultBodyLimit::max(state.limits.artifact))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_user_auth,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_node_auth,
        ))
        // Hardening P2 item 36: default security headers on every response
        // (Referrer-Policy + a restrictive Permissions-Policy). HSTS is opt-in
        // via AGENTGRID_HSTS=1 so a plain-HTTP-control-plane-terminated TLS at
        // a reverse proxy does not pin a self-signed cert. The per-route CSP
        // (set on the SPA shell + artifact responses) stays untouched.
        .layer(axum::middleware::from_fn(
            middleware::security_headers_middleware,
        ))
        // Hardening P2 item 19/35: outermost layer so every log line inside a
        // request (auth, handler, store) carries a stable `request_id`. The
        // JSON formatter attaches the current span to each event, so the id is
        // correlatable across the whole request without per-handler plumbing.
        .layer(axum::middleware::from_fn(middleware::request_id_middleware))
        .fallback(middleware::spa_fallback)
        .with_state(state)
}

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
    let nodes = match state.store.list_nodes(None, None).await {
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

    // Hardening P2 item 35: validation duration histogram + outcomes.
    s.push_str(
        "# HELP agentgrid_validation_duration_ms Validation duration (validating-state window).\n",
    );
    s.push_str("# TYPE agentgrid_validation_duration_ms histogram\n");
    s.push_str(&format!(
        "agentgrid_validation_duration_ms_sum {}\n",
        state
            .store
            .validation_duration_sum
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(&format!(
        "agentgrid_validation_duration_ms_count {}\n",
        state
            .store
            .validation_duration_count
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str("# HELP agentgrid_validation_outcomes_total Validation outcomes.\n");
    s.push_str("# TYPE agentgrid_validation_outcomes_total counter\n");
    for (k, v) in state.store.validation_outcomes.lock().unwrap().iter() {
        s.push_str(&format!(
            "agentgrid_validation_outcomes_total{{outcome=\"{k}\"}} {v}\n"
        ));
    }
    s.push_str(
        "# HELP agentgrid_attempts_by_security_profile_total Attempts by security profile.\n",
    );
    s.push_str("# TYPE agentgrid_attempts_by_security_profile_total counter\n");
    for (k, v) in state.store.security_profile_attempts.lock().unwrap().iter() {
        s.push_str(&format!(
            "agentgrid_attempts_by_security_profile_total{{profile=\"{k}\"}} {v}\n"
        ));
    }

    // Hardening P2 items 10/35: per-node storage & lock gauges from heartbeat.
    s.push_str("# HELP agentgrid_node_outbox_bytes Bytes buffered in the node's durable outbox.\n");
    s.push_str("# TYPE agentgrid_node_outbox_bytes gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_outbox_bytes{{node=\"{}\"}} {}\n",
            n.name, n.outbox_bytes
        ));
    }
    s.push_str("# HELP agentgrid_node_outbox_rows Pending outbox rows on the node.\n");
    s.push_str("# TYPE agentgrid_node_outbox_rows gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_outbox_rows{{node=\"{}\"}} {}\n",
            n.name, n.outbox_rows
        ));
    }
    s.push_str("# HELP agentgrid_node_outbox_oldest_pending_age_ms Age of the oldest unacked outbox event.\n");
    s.push_str("# TYPE agentgrid_node_outbox_oldest_pending_age_ms gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_outbox_oldest_pending_age_ms{{node=\"{}\"}} {}\n",
            n.name, n.outbox_oldest_pending_age_ms
        ));
    }
    s.push_str("# HELP agentgrid_node_outbox_corruption_total Quarantined corrupt outbox records on the node.\n");
    s.push_str("# TYPE agentgrid_node_outbox_corruption_total gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_outbox_corruption_total{{node=\"{}\"}} {}\n",
            n.name, n.outbox_corruption_count
        ));
    }
    s.push_str(
        "# HELP agentgrid_node_outbox_completion_rows Pending completion records on the node.\n",
    );
    s.push_str("# TYPE agentgrid_node_outbox_completion_rows gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_outbox_completion_rows{{node=\"{}\"}} {}\n",
            n.name, n.outbox_completion_rows
        ));
    }
    s.push_str(
        "# HELP agentgrid_node_artifact_spool_bytes Bytes staged in the node's artifact spool.\n",
    );
    s.push_str("# TYPE agentgrid_node_artifact_spool_bytes gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_artifact_spool_bytes{{node=\"{}\"}} {}\n",
            n.name, n.artifact_spool_bytes
        ));
    }
    s.push_str(
        "# HELP agentgrid_node_repo_lock_wait_ms Cumulative repository-lock wait on the node.\n",
    );
    s.push_str("# TYPE agentgrid_node_repo_lock_wait_ms gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_repo_lock_wait_ms{{node=\"{}\"}} {}\n",
            n.name, n.repo_lock_wait_ms
        ));
        s.push_str("# HELP agentgrid_node_sandbox_backend Sandbox backend kind per node.\n");
        s.push_str("# TYPE agentgrid_node_sandbox_backend gauge\n");
        for n in &nodes {
            s.push_str(&format!(
                "agentgrid_node_sandbox_backend{{node=\"{}\",backend=\"{}\"}} 1\n",
                n.name, n.sandbox_backend
            ));
        }
        s.push_str(
            "# HELP agentgrid_node_enforced_limits Whether sandbox enforces resource limits.\n",
        );
        s.push_str("# TYPE agentgrid_node_enforced_limits gauge\n");
        for n in &nodes {
            s.push_str(&format!(
                "agentgrid_node_enforced_limits{{node=\"{}\"}} {}\n",
                n.name,
                if n.enforced_limits { 1 } else { 0 }
            ));
        }
        // Hardening P2 item 659: node network mode
        s.push_str(
            "# HELP agentgrid_node_network_mode Network mode per node.
",
        );
        s.push_str(
            "# TYPE agentgrid_node_network_mode gauge
",
        );
        for n in &nodes {
            let mode = match n.network_mode.as_str() {
                "none" => 0,
                "restricted" => 1,
                "unrestricted" => 2,
                _ => 0,
            };
            let labels = format!("node=\"{}\",mode=\"{}\"", n.name, n.network_mode);
            s.push_str(&format!(
                "agentgrid_node_network_mode{{{}}} {}",
                labels, mode
            ));
        }
    }

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

// ----- workflows (Stage 7.2) -----

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
            let acceptor = tls::load_tls_acceptor(&cert, &key)?;
            tracing::info!("control plane listening with TLS on {addr}");
            axum::serve(
                tls::TlsListener {
                    tcp: listener,
                    acceptor,
                },
                app,
            )
            .with_graceful_shutdown(tls::shutdown_signal(state.clone()))
            .await?;
        }
        _ => {
            tracing::info!("control plane listening on {addr} (plaintext)");
            axum::serve(listener, app)
                .with_graceful_shutdown(tls::shutdown_signal(state.clone()))
                .await?;
        }
    }
    Ok(())
}
