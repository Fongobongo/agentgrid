//! Control plane for agentgrid.
//!
//! HTTP surface (`/v1`) and long-poll scheduler are stable; the backing store
//! is SQLite (see [`store`]). Stage 1 used an in-memory map — swapped for
//! persistence in Stage 2.1.

mod auth;
use auth::Claims;
mod config;
mod middleware;
mod notify;
mod routes;
mod services;
pub mod store;
mod tls;
pub mod workflow;
pub mod ws;

// OpenTelemetry metrics (optional feature)
pub mod otel;

use crate::config::{env_usize, EventRate, Limits, LoginRate, SetupToken, SETUP_TOKEN_TTL};
use crate::store::is_safe_opaque_id;
use std::sync::Arc;

use axum::{
    extract::DefaultBodyLimit,
    routing::{delete, get, post},
    Router,
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
    /// Plan 534: cross-aggregate workflow orchestration (attempt completion →
    /// workflow run advance). Handlers call the service instead of the store
    /// when the operation spans aggregates.
    pub lifecycle: services::TaskLifecycleService,
    /// Plan 533: node poll orchestration (degrade + touch + assign).
    pub scheduler: services::SchedulerService,
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
    /// Plan 0.3 2.2: connected WS nodes (ADR 0009). The pump task pushes
    /// assignments through this registry; poll-based nodes never touch it.
    pub ws_registry: std::sync::Arc<ws::WsRegistry>,
    /// Plan 1.2 (#22a): optional webhook invoked on terminal/operator-facing
    /// task events (completed, failed, awaiting review). Compatible with
    /// ntfy.sh / Telegram bot API / FCM legacy. Disabled when unset.
    pub notify_webhook: Option<String>,
    /// Plan 1.8 (#15): per-node account usage reported in heartbeats, kept in
    /// memory (no schema change) for `GET /v1/nodes/{id}/accounts/usage`.
    pub account_usage: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, Vec<agentgrid_common::AccountUsage>>>,
    >,
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
        let notify_webhook = std::env::var("AGENTGRID_NOTIFY_WEBHOOK")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let limits = Limits {
            prompt: env_usize("AGENTGRID_MAX_PROMPT_KB", 64) * 1024,
            event: env_usize("AGENTGRID_MAX_EVENT_KB", 1024) * 1024,
            artifact: env_usize("AGENTGRID_MAX_ARTIFACT_MB", 50) * 1024 * 1024,
            event_batch_count: env_usize("AGENTGRID_MAX_EVENT_BATCH", 500),
            event_batch_bytes: env_usize("AGENTGRID_MAX_EVENT_BATCH_KB", 4096) * 1024,
            // Hardening P1 item 15 (0 = unlimited).
            artifact_quota_bytes: std::sync::atomic::AtomicU64::new(
                std::env::var("AGENTGRID_ARTIFACT_QUOTA_MB")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0)
                    .saturating_mul(1024 * 1024),
            ),
        };
        let assignment_notify = Arc::new(Notify::new());
        let lifecycle = services::TaskLifecycleService::new(
            store.clone(),
            assignment_notify.clone(),
            notify_webhook.clone(),
        );
        let scheduler = services::SchedulerService::new(store.clone(), assignment_notify.clone());
        let state = Arc::new(Self {
            store,
            lifecycle,
            scheduler,
            assignment_notify,
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
            ws_registry: std::sync::Arc::new(ws::WsRegistry::new()),
            notify_webhook,
            account_usage: std::sync::Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
        });
        // Plan 0.3 2.2: push assignments to connected WS nodes on every
        // scheduler wake. Harmless when no node is connected (tests).
        ws::start_pump(state.clone());
        Ok(state)
    }

    /// Open a fresh temporary database with no users (used by tests that
    /// exercise the bootstrap/setup flow). A one-time setup token is minted
    /// and printed to stdout (same as a fresh install).
    pub async fn open_temp_fresh() -> anyhow::Result<Arc<Self>> {
        // Disable critical disk watermark for tests (temp fs often has < 512 MB free).
        std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
        let p = std::env::temp_dir().join(format!("ag-test-{}.db", Uuid::new_v4()));
        Self::open(p.to_str().unwrap()).await
    }

    /// Open a fresh temporary database (used by tests). Bootstraps a
    /// `test`/`test` user so the closed bootstrap window does not block
    /// test task creation; tests then login to obtain a JWT.
    pub async fn open_temp() -> anyhow::Result<Arc<Self>> {
        // Disable critical disk watermark for tests (temp fs often has < 512 MB free).
        std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
        let p = std::env::temp_dir().join(format!("ag-test-{}.db", Uuid::new_v4()));
        let state = Self::open(p.to_str().unwrap()).await?;
        if state.store.user_count().await? == 0 {
            state
                .store
                .create_user("test", "test", agentgrid_common::ROLE_ADMIN)
                .await?;
        }
        Ok(state)
    }

    /// Replace the per-node event-ingest rate limits on this state without
    /// touching the process env. Tests use this instead of setting
    /// `AGENTGRID_EVENT_RATE_*`: env is process-global, and any test that
    /// constructs an AppState while a mutated value is visible inherits it
    /// (the source of cross-test 429 flakes — see `EventRate::with_limits`).
    pub async fn set_event_rate_limits(&self, max: u32, window_secs: i64) {
        *self.event_rate.lock().await = EventRate::with_limits(max, window_secs);
    }

    /// Override the artifact storage quota (bytes) on this state. Tests use
    /// this instead of re-reading `AGENTGRID_ARTIFACT_QUOTA_MB` per upload —
    /// the quota is captured once in `Limits` at startup.
    pub fn set_artifact_quota_bytes(&self, bytes: u64) {
        self.limits
            .artifact_quota_bytes
            .store(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// Issue a 12h JWT for `username` (Stage 4.1).
    /// Includes `jti` for session revocation (Stage 4.2) and the RBAC
    /// `role` claim (plan 5.2).
    pub(crate) fn issue_token(&self, username: &str, role: &str) -> anyhow::Result<String> {
        let exp = (chrono::Utc::now() + chrono::Duration::hours(12)).timestamp() as usize;
        let jti = Uuid::new_v4().to_string();
        let claims = Claims {
            sub: username.to_string(),
            exp,
            jti,
            role: role.to_string(),
        };
        Ok(encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(&self.jwt_secret),
        )?)
    }

    /// Validate a JWT and return the username and role, or None if
    /// revoked/invalid. Checks revoked_sessions blocklist (Stage 4.2).
    /// A storage failure is fail-closed (None → 401) like a revocation, but
    /// it is logged — silently mapping every DB blip to mass 401s hid the
    /// root cause from operators.
    pub(crate) async fn verify_token(&self, token: &str) -> Option<(String, String)> {
        let claims = decode::<Claims>(
            token,
            &DecodingKey::from_secret(&self.jwt_secret),
            &Validation::default(),
        )
        .ok()?;
        // Check if this jti has been revoked
        match self.store.is_session_revoked(&claims.claims.jti).await {
            Ok(true) => None,
            Ok(false) => Some((claims.claims.sub, claims.claims.role)),
            Err(e) => {
                tracing::error!("session revocation check failed (failing closed): {e}");
                None
            }
        }
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

impl Drop for AppState {
    /// Test hygiene: `open_temp*` databases live in the temp dir and would
    /// otherwise accumulate one db + wal + shm set per test. Unlink is safe
    /// while the pool is still open on Linux; the inode vanishes on close.
    fn drop(&mut self) {
        let prefix = std::env::temp_dir().join("ag-test-");
        if self.db_path.starts_with(prefix.to_str().unwrap()) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{}", self.db_path, suffix));
            }
        }
    }
}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health/live", get(auth::health_live))
        .route("/health/ready", get(auth::health_ready))
        .route("/metrics", get(routes::maintenance::metrics))
        .route(
            "/v1/tasks",
            post(routes::tasks::create_task).get(routes::tasks::list_tasks),
        )
        .route("/v1/tasks/{id}", get(routes::tasks::show_task))
        .route("/v1/attempts/{id}", get(routes::tasks::show_attempt))
        .route(
            "/v1/attempts/{id}/annotations",
            get(routes::tasks::list_annotations).post(routes::tasks::add_annotation),
        )
        .route(
            "/v1/attempts/{id}/rework",
            post(routes::tasks::rework_attempt),
        )
        .route("/v1/tasks/{id}/tags", get(routes::tasks::list_task_tags))
        .route(
            "/v1/tasks/{id}/tags/{tag}",
            post(routes::tasks::add_task_tag).delete(routes::tasks::remove_task_tag),
        )
        .route(
            "/v1/task-groups/{id}/context",
            get(routes::shared_context::list_context),
        )
        .route(
            "/v1/task-groups/{id}/context/{key}",
            get(routes::shared_context::get_context)
                .put(routes::shared_context::set_context)
                .delete(routes::shared_context::delete_context),
        )
        .route(
            "/v1/repos/{repo}/learnings",
            get(routes::learnings::list_learnings).post(routes::learnings::add_learning),
        )
        .route(
            "/v1/learnings/{id}/approve",
            post(routes::learnings::approve_learning),
        )
        .route(
            "/v1/learnings/{id}",
            axum::routing::delete(routes::learnings::delete_learning),
        )
        .route(
            "/v1/agents",
            post(routes::agents::create_agent).get(routes::agents::list_agents),
        )
        .route(
            "/v1/agents/{id}/actions",
            get(routes::agents::agent_actions),
        )
        .route(
            "/v1/agents/{id}/tasks",
            post(routes::agents::create_agent_task),
        )
        .route(
            "/v1/webhooks/github/issues",
            post(routes::webhooks::github_issue_webhook),
        )
        .route(
            "/v1/webhooks/github/check_run",
            post(routes::webhooks::github_check_run_webhook),
        )
        .route(
            "/v1/webhooks/github/pull_request",
            post(routes::webhooks::github_pull_request_webhook),
        )
        .route("/v1/search", get(routes::tasks::search_tasks))
        .route("/v1/search/events", get(routes::tasks::search_events))
        .route("/v1/tasks/{id}/events", get(routes::events::get_events))
        .route(
            "/v1/tasks/{id}/events/stream",
            get(routes::events::events_stream),
        )
        .route("/v1/stream", get(routes::events::changes_stream))
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
        .route("/v1/audit", get(routes::nodes::list_audit_handler))
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
        // Competitor plan 1.1: pending patch-review approval lookup for the
        // diff review UI.
        .route(
            "/v1/tasks/{id}/review-approval",
            get(routes::tasks::get_task_review_approval_handler),
        )
        .route("/v1/auth/setup", post(auth::auth_setup))
        .route("/v1/auth/login", post(auth::auth_login))
        .route("/v1/auth/logout", post(auth::auth_logout))
        .route(
            "/v1/users",
            get(routes::users::list_users_handler).post(routes::users::create_user_handler),
        )
        .route(
            "/v1/policy/evaluate",
            post(routes::profiles::evaluate_policy),
        )
        .route(
            "/v1/skills",
            get(routes::profiles::list_skills_trust_handler),
        )
        .route(
            "/v1/skills/{name}",
            get(routes::profiles::get_skill_trust_handler),
        )
        .route(
            "/v1/skills/{name}/trust",
            post(routes::profiles::trust_skill_handler),
        )
        .route(
            "/v1/skills/{name}/untrust",
            post(routes::profiles::untrust_skill_handler),
        )
        .route(
            "/v1/mcp-servers",
            get(routes::profiles::list_mcp_servers_handler)
                .post(routes::profiles::create_mcp_server_handler),
        )
        .route(
            "/v1/mcp-servers/{id}",
            delete(routes::profiles::delete_mcp_server_handler),
        )
        .route("/v1/profiles", get(routes::profiles::list_profiles_handler))
        .route(
            "/v1/profiles/{id}",
            get(routes::profiles::get_profile_handler),
        )
        .route(
            "/v1/profiles/{id}",
            post(routes::profiles::create_profile_handler),
        )
        .route(
            "/v1/profiles/{id}/activate",
            post(routes::profiles::activate_profile_handler),
        )
        .route("/v1/admin/backup", post(routes::maintenance::admin_backup))
        .route(
            "/v1/admin/storage-gc",
            post(routes::maintenance::storage_gc_handler),
        )
        .route("/v1/nodes", get(routes::nodes::list_nodes))
        .route(
            "/v1/nodes/enrollment-token",
            post(routes::nodes::create_enrollment_token),
        )
        .route(
            "/v1/nodes/{id}",
            get(routes::nodes::get_node).delete(routes::nodes::revoke_node),
        )
        .route(
            "/v1/nodes/{id}/drain",
            post(routes::nodes::drain_node_handler),
        )
        .route(
            "/v1/opencode-profiles",
            get(routes::opencode::list_profiles),
        )
        .route(
            "/v1/opencode-profiles/{name}",
            get(routes::opencode::get_profile)
                .put(routes::opencode::upsert_profile)
                .delete(routes::opencode::delete_profile),
        )
        .route(
            "/v1/opencode-profiles/{name}/rollback",
            post(routes::opencode::rollback_profile),
        )
        .route(
            "/v1/opencode-profiles/{name}/assign-percent",
            post(routes::opencode::assign_percent),
        )
        .route(
            "/v1/nodes/{id}/opencode-profile",
            post(routes::opencode::assign_node_profile),
        )
        .route(
            "/v1/nodes/{id}/opencode-audit",
            get(routes::opencode::list_audit),
        )
        .route(
            "/v1/node/opencode-config/active",
            get(routes::opencode::get_active_config),
        )
        .route(
            "/v1/node/opencode-config/audit",
            post(routes::opencode::record_audit),
        )
        .route(
            "/v1/node/skills-trust",
            get(routes::opencode::node_skills_trust),
        )
        .route(
            "/v1/nodes/{id}/accounts/usage",
            get(routes::nodes::node_account_usage),
        )
        .route(
            "/v1/repositories",
            post(routes::repositories::create_repository)
                .get(routes::repositories::list_repositories),
        )
        .route("/v1/node/enroll", post(routes::nodes::enroll))
        .route("/v1/node/poll", post(routes::events::poll))
        .route("/v1/node/ws", get(ws::node_ws))
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
            "/v1/tasks/{id}/artifacts",
            get(routes::artifacts::list_artifacts),
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
    state.store.start_agent_heartbeat_ticker();
    // Configurator polish: opencode-profile TTL janitor — every 15 s sweep
    // expired profiles (same semantics as a manual DELETE) and wake their
    // nodes with a ConfigUpdate clear push so they drop the profile.
    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                match state.store.expire_opencode_profiles().await {
                    Ok(expired) => {
                        for (name, nodes) in expired {
                            tracing::info!(profile = %name, nodes = nodes.len(), "opencode profile expired");
                            for node_id in &nodes {
                                state
                                    .ws_registry
                                    .send(
                                        node_id,
                                        &agentgrid_common::ws::NodeWsMsg::ConfigUpdate {
                                            profile_id: None,
                                            hash: None,
                                        },
                                    )
                                    .await;
                            }
                        }
                    }
                    Err(e) => tracing::warn!("opencode profile expiry sweep failed: {e}"),
                }
            }
        });
    }
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
