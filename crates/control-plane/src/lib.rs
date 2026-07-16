//! Control plane for agentgrid.
//!
//! HTTP surface (`/v1`) and long-poll scheduler are stable; the backing store
//! is SQLite (see [`store`]). Stage 1 used an in-memory map — swapped for
//! persistence in Stage 2.1.

pub mod store;

use std::sync::Arc;
use std::time::Instant;

use agentgrid_common::{
    CancelState, CompleteAttemptRequest, CreateRepositoryRequest, CreateTaskRequest, EnrollRequest,
    EnrollResponse, EnrollTokenResponse, EventsQuery, HeartbeatRequest, IngestEventsRequest,
    LoginRequest, LoginResponse, PollRequest, PollResponse, RepositoryView, SetupRequest,
    TaskEligibility, TaskView, UploadArtifactRequest,
};
use axum::{
    body::Body,
    extract::{Extension, Path, Query, State},
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::Response,
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

pub struct AppState {
    pub store: Store,
    assignment_notify: Arc<Notify>,
    jwt_secret: Vec<u8>,
}

impl AppState {
    /// Open (or create) the SQLite database at `db_path` and return shared state.
    pub async fn open(db_path: &str) -> anyhow::Result<Arc<Self>> {
        let store = Store::open(db_path).await?;
        let jwt_secret = std::env::var("AGENTGRID_JWT_SECRET")
            .map(|s| s.into_bytes())
            .unwrap_or_else(|_| {
                use rand::Rng;
                rand::thread_rng().gen::<[u8; 32]>().to_vec()
            });
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
        Ok(Arc::new(Self {
            store,
            assignment_notify: Arc::new(Notify::new()),
            jwt_secret,
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
        .route("/v1/auth/setup", post(auth_setup))
        .route("/v1/auth/login", post(auth_login))
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
        .route("/v1/node/attempts/{id}/artifacts", post(upload_artifact))
        .route("/v1/tasks/{id}/artifacts/{name}", get(get_artifact))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_user_auth,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_node_auth,
        ))
        .with_state(state)
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
    if path == "/v1/auth/login" || path == "/v1/auth/setup" {
        return false;
    }
    true
}

/// Require a valid user JWT on user-facing routes, except during the open
/// bootstrap window (no users yet). Node routes are handled by
/// [`require_node_auth`] and are skipped here.
async fn require_user_auth(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let path = req.uri().path().to_string();
    if user_protected(&path) {
        let open = state.store.user_count().await.map(|c| c == 0).unwrap_or(true);
        if !open {
            let ok = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.strip_prefix("Bearer "))
                .and_then(|t| state.verify_token(t))
                .is_some();
            if !ok {
                return Err(StatusCode::UNAUTHORIZED);
            }
        }
    }
    Ok(next.run(req).await)
}

async fn auth_setup(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SetupRequest>,
) -> Result<(StatusCode, Json<LoginResponse>), StatusCode> {
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
    let token = state
        .issue_token(&req.username)
        .map_err(|e| {
            tracing::error!("issue_token failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok((StatusCode::CREATED, Json(LoginResponse { token })))
}

async fn auth_login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, StatusCode> {
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
    Ok(Json(LoginResponse { token }))
}

async fn health_ready(State(state): State<Arc<AppState>>) -> StatusCode {
    if state.store.health_check().await {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
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
    Json(req): Json<CreateTaskRequest>,
) -> (StatusCode, Json<TaskView>) {
    match state.store.create_task(&req).await {
        Ok(view) => {
            state.assignment_notify.notify_waiters();
            (StatusCode::CREATED, Json(view))
        }
        Err(e) => {
            tracing::error!("create_task failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(TaskView {
                    id: String::new(),
                    repository: String::new(),
                    prompt: String::new(),
                    adapter: String::new(),
                    status: agentgrid_common::TaskStatus::Queued,
                    created_at: String::new(),
                    finished_at: None,
                    assigned_attempt_id: None,
                }),
            )
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
        Ok(Some(r)) => (StatusCode::OK, Json(Some(r))),
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
    Json(req): Json<CreateRepositoryRequest>,
) -> (StatusCode, Json<RepositoryView>) {
    match state.store.create_repository(&req).await {
        Ok(v) => (StatusCode::CREATED, Json(v)),
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

async fn get_artifact(
    State(state): State<Arc<AppState>>,
    Path((task_id, name)): Path<(String, String)>,
) -> Result<String, StatusCode> {
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
    match state.store.save_artifact(&attempt_id, &req).await {
        Ok(()) => StatusCode::OK,
        Err(e) => {
            tracing::error!("save_artifact failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn events_stream(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> axum::response::sse::Sse<
    impl Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::{Event, Sse};
    use std::time::Duration;
    let mut after = q.after_sequence;
    let stream = async_stream::stream! {
        loop {
            match state.store.get_events(&task_id, after).await {
                Ok(events) if !events.is_empty() => {
                    for e in events {
                        after = after.max(e.sequence);
                        if let Ok(data) = serde_json::to_string(&e) {
                            yield Ok(Event::default().data(data));
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
    Path(task_id): Path<String>,
) -> StatusCode {
    match state.store.cancel_task(&task_id).await {
        Ok(true) => StatusCode::OK,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("cancel_task failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn retry_task_handler(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> StatusCode {
    match state.store.retry_task(&task_id).await {
        Ok(true) => StatusCode::OK,
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

/// Bind and serve. Starts background maintenance (lease/heartbeat jobs).
pub async fn serve(state: Arc<AppState>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    state.store.start_maintenance();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("control plane listening on {addr}");
    axum::serve(listener, build_router(state)).await?;
    Ok(())
}
