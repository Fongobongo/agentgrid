//! Task routes: create/list/show/eligibility/cancel/retry.

use std::sync::Arc;

use agentgrid_common::{ApprovalView, CreateTaskRequest, ListResponse, TaskEligibility, TaskView};
use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    Json,
};

use crate::auth::AuthedUser;
use crate::AppState;

pub async fn create_task(
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

/// Hardening P2 item 20: optional server-side filters + keyset cursor for
/// `GET /v1/tasks`. `after_created_at` + `after_id` form a keyset cursor
/// (rows strictly after `(created_at, id)`); `limit` caps the page (server
/// ceiling 1000). Both cursor parts must be present together.
#[derive(Debug, Default, serde::Deserialize)]
pub struct TaskListQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub after_created_at: Option<String>,
    #[serde(default)]
    pub after_id: Option<String>,
    #[serde(default)]
    pub limit: Option<u64>,
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

pub async fn list_tasks(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TaskListQuery>,
) -> Result<Json<ListResponse<TaskView>>, StatusCode> {
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
        Ok(items) => {
            let next_cursor = if items.len() == q.limit.unwrap_or(100) as usize {
                items.last().map(|t| format!("{},{}", t.created_at, t.id))
            } else {
                None
            };
            Ok(Json(ListResponse::new(items, next_cursor)))
        }
        Err(e) => {
            tracing::error!("list_tasks failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// Plan 1.3 (#6): full-text search `GET /v1/search?q=...` — FTS5 bm25
/// ranking, max 50 rows.
#[derive(Debug, serde::Deserialize)]
pub struct SearchQuery {
    pub q: String,
}

pub async fn search_tasks(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Vec<TaskView>>, StatusCode> {
    match state.store.search_tasks(&q.q).await {
        Ok(items) => Ok(Json(items)),
        Err(e) => {
            tracing::error!("search_tasks failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

pub async fn show_task(
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

/// Plan 1.3 (#13): single-attempt detail (prompt included for `ag resume`).
pub async fn show_attempt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<agentgrid_common::AttemptView>, StatusCode> {
    state
        .store
        .show_attempt(&id)
        .await
        .map_err(|e| {
            tracing::error!("show_attempt failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Plan 1.6 (#3b): leave an inline annotation on an attempt's diff/plan.
pub async fn add_annotation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<agentgrid_common::CreateAnnotationRequest>,
) -> Result<(StatusCode, Json<agentgrid_common::PatchAnnotation>), StatusCode> {
    let req = agentgrid_common::CreateAnnotationRequest {
        comment: req.comment.trim().to_string(),
        ..req
    };
    if req.comment.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    match state.store.add_annotation(&id, &req).await {
        Ok(a) => Ok((StatusCode::CREATED, Json(a))),
        Err(e) => {
            tracing::error!("add_annotation failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Plan 1.6 (#3b): list an attempt's inline annotations.
pub async fn list_annotations(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<agentgrid_common::PatchAnnotation>>, StatusCode> {
    state
        .store
        .list_annotations(&id)
        .await
        .map(Json)
        .map_err(|e| {
            tracing::error!("list_annotations failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

/// Plan 1.6 (#3b): "send for rework" — start a new task that re-runs the
/// original work with the reviewer's inline annotations folded into the
/// prompt. Returns the new task id.
pub async fn rework_attempt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<agentgrid_common::ReworkResponse>), StatusCode> {
    let origin = state
        .store
        .attempt_origin(&id)
        .await
        .map_err(|e| {
            tracing::error!("attempt_origin failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    let (prompt, repository, adapter) = origin;
    let anns = state.store.list_annotations(&id).await.map_err(|e| {
        tracing::error!("list_annotations (rework) failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let block = render_annotations_block(&anns);
    let new_prompt = format!("{prompt}\n\n{block}");
    let req = CreateTaskRequest {
        prompt: new_prompt,
        repository,
        adapter,
        requested_node_id: None,
        timeout_secs: None,
        validation_command: None,
        base_commit: None,
        parent_acp_session_id: None,
        security_profile: None,
        network_mode: None,
        group_id: None,
        agent_id: None,
        consensus_group_id: None,
        consensus_member: None,
        opencode_override: None,
    };
    match state.store.create_task(&req).await {
        Ok(view) => Ok((
            StatusCode::CREATED,
            Json(agentgrid_common::ReworkResponse { task_id: view.id }),
        )),
        Err(e) => {
            tracing::error!("rework create_task failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Plan 1.6 (#3b): compact `[ANNOTATIONS]` block appended to the prompt so the
/// agent takes the reviewer's inline feedback in a retry. Each annotation is
/// one line: `<file>[:L<L>-<L>] <comment>`. Plan 1.7 (#14): each comment runs
/// through the compress pipe (dedup consecutive identical lines + byte cap)
/// before it lands in the prompt, so a pasted log/diff in a comment does not
/// blow the token budget.
fn render_annotations_block(anns: &[agentgrid_common::PatchAnnotation]) -> String {
    if anns.is_empty() {
        return "[ANNOTATIONS] (none)".to_string();
    }
    let mut lines = vec!["[ANNOTATIONS] rework feedback from review:".to_string()];
    for a in anns {
        let loc = match (a.line_start, a.line_end) {
            (Some(s), Some(e)) if s != e => format!("{}:L{}-{}", a.file, s, e),
            (Some(s), _) => format!("{}:L{}", a.file, s),
            _ => a.file.clone(),
        };
        // 4096-byte cap per comment — pasted logs collapse via dedup + truncate.
        let (comment, _) = agentgrid_common::compress::compress(&a.comment, 4096);
        lines.push(format!("- {loc} {}", comment));
    }
    lines.join("\n")
}

/// Plan 1.3 (#13): tag CRUD — `GET /v1/tasks/{id}/tags`,
/// `POST /v1/tasks/{id}/tags/{tag}`, `DELETE /v1/tasks/{id}/tags/{tag}`.
pub async fn list_task_tags(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<String>>, StatusCode> {
    state.store.list_tags(&id).await.map(Json).map_err(|e| {
        tracing::error!("list_tags failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

pub async fn add_task_tag(
    State(state): State<Arc<AppState>>,
    Path((id, tag)): Path<(String, String)>,
) -> Result<StatusCode, StatusCode> {
    state.store.add_tag(&id, &tag).await.map_err(|e| {
        tracing::error!("add_tag failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn remove_task_tag(
    State(state): State<Arc<AppState>>,
    Path((id, tag)): Path<(String, String)>,
) -> Result<StatusCode, StatusCode> {
    state.store.remove_tag(&id, &tag).await.map_err(|e| {
        tracing::error!("remove_tag failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn task_eligibility_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<TaskEligibility>, StatusCode> {
    match state.store.task_eligibility(&id).await {
        Ok(Some(elig)) => Ok(Json(elig)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("task_eligibility failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

pub async fn cancel_task_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Path(task_id): Path<String>,
) -> StatusCode {
    match state.store.cancel_task(&task_id).await {
        Ok(true) => {
            // Plan 0.3 2.2: push the cancel to the owning WS node (best
            // effort; the store flag stays authoritative for poll nodes).
            crate::ws::push_cancel_for_task(&state, &task_id).await;
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

/// Competitor plan 1.1: expose the pending patch-review approval for a task.
/// 200 with `null` when there is nothing to review; the UI uses this to
/// decide whether to show the approve/reject/rework buttons.
pub async fn get_task_review_approval_handler(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Json<Option<ApprovalView>>, StatusCode> {
    match state.store.find_pending_patch_review(&task_id).await {
        Ok(a) => Ok(Json(a)),
        Err(e) => {
            tracing::error!("get_task_review_approval failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

pub async fn retry_task_handler(
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
