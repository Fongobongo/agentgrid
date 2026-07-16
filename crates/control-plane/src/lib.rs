//! In-memory control plane for the MVP vertical prototype (Stage 1).
//!
//! Persists nothing; state lives in a `tokio::sync::Mutex`. This is replaced
//! by a SQLite-backed storage layer in Stage 2. The HTTP surface and DTOs are
//! stable across that swap.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentgrid_common::{
    Assignment, AttemptStatus, CompleteAttemptRequest, CreateTaskRequest, IngestEventsRequest,
    NodeStatus, NodeView, PollRequest, PollResponse, TaskEvent, TaskStatus, TaskView,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

const POLL_TIMEOUT: Duration = Duration::from_secs(25);

struct Node {
    id: String,
    name: String,
    status: NodeStatus,
    adapters: Vec<String>,
    repositories: Vec<String>,
    max_concurrency: u32,
    active_attempts: u32,
    last_heartbeat_at: String,
}

struct Task {
    id: String,
    repository: String,
    prompt: String,
    adapter: String,
    requested_node_id: Option<String>,
    status: TaskStatus,
    created_at: String,
    finished_at: Option<String>,
    assigned_attempt_id: Option<String>,
}

#[allow(dead_code)]
struct Attempt {
    id: String,
    task_id: String,
    node_id: String,
    number: u32,
    status: AttemptStatus,
    exit_code: Option<i32>,
    started_at: String,
    finished_at: Option<String>,
}

struct Inner {
    nodes: HashMap<String, Node>,
    tasks: HashMap<String, Task>,
    attempts: HashMap<String, Attempt>,
    events: HashMap<String, Vec<TaskEvent>>,
}

pub struct AppState {
    inner: Mutex<Inner>,
    assignment_notify: Arc<Notify>,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                nodes: HashMap::new(),
                tasks: HashMap::new(),
                attempts: HashMap::new(),
                events: HashMap::new(),
            }),
            assignment_notify: Arc::new(Notify::new()),
        })
    }
}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        .route("/v1/tasks", post(create_task).get(list_tasks))
        .route("/v1/tasks/{id}", get(show_task))
        .route("/v1/tasks/{id}/events", get(get_events))
        .route("/v1/nodes", get(list_nodes))
        .route("/v1/node/poll", post(poll))
        .route("/v1/node/attempts/{id}/events", post(ingest_events))
        .route("/v1/node/attempts/{id}/complete", post(complete_attempt))
        .with_state(state)
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

async fn health_live() -> StatusCode {
    StatusCode::OK
}

async fn health_ready() -> StatusCode {
    StatusCode::OK
}

fn task_view(t: &Task) -> TaskView {
    TaskView {
        id: t.id.clone(),
        repository: t.repository.clone(),
        prompt: t.prompt.clone(),
        adapter: t.adapter.clone(),
        status: t.status,
        created_at: t.created_at.clone(),
        finished_at: t.finished_at.clone(),
        assigned_attempt_id: t.assigned_attempt_id.clone(),
    }
}

async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateTaskRequest>,
) -> (StatusCode, Json<TaskView>) {
    let id = Uuid::new_v4().to_string();
    let created_at = now_iso();
    let task = Task {
        id: id.clone(),
        repository: req.repository,
        prompt: req.prompt,
        adapter: req.adapter,
        requested_node_id: req.requested_node_id,
        status: TaskStatus::Queued,
        created_at: created_at.clone(),
        finished_at: None,
        assigned_attempt_id: None,
    };
    state.inner.lock().await.tasks.insert(id.clone(), task);
    // Wake any polling node so it can pick up the new queued task.
    state.assignment_notify.notify_waiters();
    (
        StatusCode::CREATED,
        Json(TaskView {
            id,
            repository: String::new(),
            prompt: String::new(),
            adapter: String::new(),
            status: TaskStatus::Queued,
            created_at,
            finished_at: None,
            assigned_attempt_id: None,
        }),
    )
}

async fn list_tasks(State(state): State<Arc<AppState>>) -> Json<Vec<TaskView>> {
    let inner = state.inner.lock().await;
    let mut out: Vec<TaskView> = inner.tasks.values().map(task_view).collect();
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Json(out)
}

async fn show_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<TaskView>, StatusCode> {
    let inner = state.inner.lock().await;
    inner
        .tasks
        .get(&id)
        .map(|t| Json(task_view(t)))
        .ok_or(StatusCode::NOT_FOUND)
}

async fn list_nodes(State(state): State<Arc<AppState>>) -> Json<Vec<NodeView>> {
    let inner = state.inner.lock().await;
    let out = inner
        .nodes
        .values()
        .map(|n| NodeView {
            id: n.id.clone(),
            name: n.name.clone(),
            status: n.status,
            adapters: n.adapters.clone(),
            repositories: n.repositories.clone(),
            max_concurrency: n.max_concurrency,
            active_attempts: n.active_attempts,
            last_heartbeat_at: n.last_heartbeat_at.clone(),
        })
        .collect();
    Json(out)
}

async fn get_events(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
    Query(q): Query<agentgrid_common::EventsQuery>,
) -> Json<Vec<TaskEvent>> {
    let inner = state.inner.lock().await;
    let attempt_ids: Vec<String> = inner
        .attempts
        .values()
        .filter(|a| a.task_id == task_id)
        .map(|a| a.id.clone())
        .collect();
    let mut evs: Vec<TaskEvent> = Vec::new();
    for aid in attempt_ids {
        if let Some(list) = inner.events.get(&aid) {
            for e in list {
                if e.sequence > q.after_sequence {
                    evs.push(e.clone());
                }
            }
        }
    }
    evs.sort_by_key(|e| (e.attempt_id.clone(), e.sequence));
    Json(evs)
}

async fn poll(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PollRequest>,
) -> (StatusCode, Json<PollResponse>) {
    {
        let mut inner = state.inner.lock().await;
        match inner.nodes.get_mut(&req.node_id) {
            Some(n) => {
                n.name = req.name.clone();
                n.adapters = req.adapters.clone();
                n.repositories = req.repositories.clone();
                n.max_concurrency = req.max_concurrency;
                n.last_heartbeat_at = now_iso();
                if n.status == NodeStatus::Offline || n.status == NodeStatus::Pending {
                    n.status = NodeStatus::Online;
                }
            }
            None => {
                inner.nodes.insert(
                    req.node_id.clone(),
                    Node {
                        id: req.node_id.clone(),
                        name: req.name.clone(),
                        status: NodeStatus::Online,
                        adapters: req.adapters.clone(),
                        repositories: req.repositories.clone(),
                        max_concurrency: req.max_concurrency,
                        active_attempts: 0,
                        last_heartbeat_at: now_iso(),
                    },
                );
            }
        }
    }

    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        {
            let mut inner = state.inner.lock().await;
            if let Some(assignment) = try_assign(&mut inner, &req.node_id) {
                return (
                    StatusCode::OK,
                    Json(PollResponse {
                        assignment: Some(assignment),
                    }),
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

/// First-fit scheduler: pick a queued task whose adapter + repository match
/// the node capabilities, respecting `requested_node_id` and capacity.
fn try_assign(inner: &mut Inner, node_id: &str) -> Option<Assignment> {
    let mut chosen_id: Option<String> = None;
    for t in inner.tasks.values() {
        if t.status != TaskStatus::Queued {
            continue;
        }
        if let Some(req) = &t.requested_node_id {
            if req != node_id {
                continue;
            }
        }
        let node = inner.nodes.get(node_id)?;
        if node.status != NodeStatus::Online {
            return None;
        }
        if node.active_attempts >= node.max_concurrency {
            return None;
        }
        if !node.adapters.contains(&t.adapter) {
            continue;
        }
        let repo_ok = node
            .repositories
            .iter()
            .any(|r| r == "*" || r == &t.repository);
        if !repo_ok {
            continue;
        }
        chosen_id = Some(t.id.clone());
        break;
    }
    let task_id = chosen_id?;

    let number = inner
        .attempts
        .values()
        .filter(|a| a.task_id == task_id)
        .count() as u32
        + 1;
    let attempt_id = Uuid::new_v4().to_string();
    let now = now_iso();

    let assignment;
    {
        let task = inner.tasks.get_mut(&task_id).unwrap();
        assignment = Assignment {
            attempt_id: attempt_id.clone(),
            task_id: task.id.clone(),
            repository: task.repository.clone(),
            prompt: task.prompt.clone(),
            adapter: task.adapter.clone(),
            number,
        };
        task.status = TaskStatus::Assigned;
        task.assigned_attempt_id = Some(attempt_id.clone());
    }
    if let Some(node) = inner.nodes.get_mut(node_id) {
        node.active_attempts += 1;
    }
    inner.attempts.insert(
        attempt_id.clone(),
        Attempt {
            id: attempt_id.clone(),
            task_id: task_id.clone(),
            node_id: node_id.to_string(),
            number,
            status: AttemptStatus::Assigned,
            exit_code: None,
            started_at: now,
            finished_at: None,
        },
    );
    Some(assignment)
}

async fn ingest_events(
    State(state): State<Arc<AppState>>,
    Path(attempt_id): Path<String>,
    Json(req): Json<IngestEventsRequest>,
) -> StatusCode {
    let mut inner = state.inner.lock().await;
    let task_id = {
        let attempt = match inner.attempts.get_mut(&attempt_id) {
            Some(a) => a,
            None => return StatusCode::NOT_FOUND,
        };
        if attempt.status == AttemptStatus::Assigned {
            attempt.status = AttemptStatus::Running;
            Some(attempt.task_id.clone())
        } else {
            None
        }
    };
    if let Some(tid) = task_id {
        if let Some(task) = inner.tasks.get_mut(&tid) {
            task.status = TaskStatus::Running;
        }
    }
    let now = now_iso();
    let entry = inner.events.entry(attempt_id.clone()).or_default();
    for ev in req.events {
        if entry.iter().any(|e| e.sequence == ev.sequence) {
            continue; // idempotent
        }
        entry.push(TaskEvent {
            attempt_id: attempt_id.clone(),
            sequence: ev.sequence,
            r#type: ev.r#type,
            payload: ev.payload,
            created_at: now.clone(),
        });
    }
    entry.sort_by_key(|e| e.sequence);
    StatusCode::OK
}

async fn complete_attempt(
    State(state): State<Arc<AppState>>,
    Path(attempt_id): Path<String>,
    Json(req): Json<CompleteAttemptRequest>,
) -> StatusCode {
    let mut inner = state.inner.lock().await;
    let (task_id, node_id, finished_at) = {
        let attempt = match inner.attempts.get_mut(&attempt_id) {
            Some(a) => a,
            None => return StatusCode::NOT_FOUND,
        };
        attempt.exit_code = Some(req.exit_code);
        attempt.status = if req.exit_code == 0 {
            AttemptStatus::Succeeded
        } else {
            AttemptStatus::Failed
        };
        let finished = now_iso();
        attempt.finished_at = Some(finished.clone());
        (attempt.task_id.clone(), attempt.node_id.clone(), finished)
    };
    if let Some(task) = inner.tasks.get_mut(&task_id) {
        task.status = if req.exit_code == 0 {
            TaskStatus::Succeeded
        } else {
            TaskStatus::Failed
        };
        task.finished_at = Some(finished_at);
    }
    if let Some(node) = inner.nodes.get_mut(&node_id) {
        node.active_attempts = node.active_attempts.saturating_sub(1);
    }
    StatusCode::OK
}

/// Bind and serve. Used by `main.rs` and integration tests build their own
/// router via [`build_router`].
pub async fn serve(state: Arc<AppState>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("control plane listening on {addr}");
    axum::serve(listener, build_router(state)).await?;
    Ok(())
}
