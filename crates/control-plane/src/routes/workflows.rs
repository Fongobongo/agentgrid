//! Workflow routes: templates, runs, schedules, projection, tick.

use std::sync::Arc;

use agentgrid_common::{
    CreateWorkflowRequest, CreateWorkflowRunRequest, ListResponse, WorkflowProjection, WorkflowRun,
    WorkflowRunWithSteps, WorkflowSchedule, WorkflowScheduleCreate, WorkflowTemplate,
};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};

use crate::routes::WorkflowRunsQuery;
use crate::AppState;

pub async fn create_workflow(
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

pub async fn list_workflows(
    State(state): State<Arc<AppState>>,
    Query(q): Query<WorkflowRunsQuery>,
) -> Result<Json<ListResponse<WorkflowTemplate>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — surface
    // storage outage as 503 so the client does not read it as "no workflows".
    let after = match (&q.after_created_at, &q.after_id) {
        (Some(c), Some(i)) if !c.is_empty() && !i.is_empty() => Some((c.clone(), i.clone())),
        _ => None,
    };
    match state.store.list_workflow_templates(after, q.limit).await {
        Ok(items) => {
            let next_cursor = if items.len() == q.limit.unwrap_or(100) as usize {
                items.last().map(|t| format!("{},{}", t.created_at, t.id))
            } else {
                None
            };
            Ok(Json(ListResponse::new(items, next_cursor)))
        }
        Err(e) => {
            tracing::error!("list_workflows failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

pub async fn show_workflow(
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

pub async fn create_workflow_run(
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
pub async fn create_workflow_schedule(
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

pub async fn list_workflow_schedules(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<WorkflowRunsQuery>,
) -> Result<Json<ListResponse<WorkflowSchedule>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — surface
    // storage outage as 503 so the client does not read it as "no schedules".
    let after = match (&q.after_created_at, &q.after_id) {
        (Some(c), Some(i)) if !c.is_empty() && !i.is_empty() => Some((c.clone(), i.clone())),
        _ => None,
    };
    match state
        .store
        .list_workflow_schedules(Some(&id), after, q.limit)
        .await
    {
        Ok(items) => {
            let next_cursor = if items.len() == q.limit.unwrap_or(100) as usize {
                items.last().map(|s| format!("{},{}", s.created_at, s.id))
            } else {
                None
            };
            Ok(Json(ListResponse::new(items, next_cursor)))
        }
        Err(e) => {
            tracing::error!("list_workflow_schedules failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

pub async fn delete_workflow_schedule(
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

pub async fn list_workflow_runs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<WorkflowRunsQuery>,
) -> Result<Json<ListResponse<WorkflowRun>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — surface
    // storage outage as 503 so the client does not read it as "no runs".
    let after = match (&q.after_created_at, &q.after_id) {
        (Some(c), Some(i)) if !c.is_empty() && !i.is_empty() => Some((c.clone(), i.clone())),
        _ => None,
    };
    match state.store.list_workflow_runs(after, q.limit).await {
        Ok(items) => {
            let next_cursor = if items.len() == q.limit.unwrap_or(100) as usize {
                items.last().map(|r| format!("{},{}", r.created_at, r.id))
            } else {
                None
            };
            Ok(Json(ListResponse::new(items, next_cursor)))
        }
        Err(e) => {
            tracing::error!("list_workflow_runs failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

pub async fn show_workflow_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<WorkflowRunWithSteps>, StatusCode> {
    // Plan 536: single store call — run + steps read together, no handler
    // coordination of two reads.
    match state.store.get_workflow_run_with_steps(&id).await {
        Ok(Some((r, s))) => Ok(Json(WorkflowRunWithSteps { run: r, steps: s })),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        _ => {
            tracing::error!("show_workflow_run failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Stage 8 ACP plan projection: live roles/steps/nodes/verdicts for a run.
pub async fn workflow_run_projection(
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

pub async fn tick_workflow_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<WorkflowRunWithSteps>, StatusCode> {
    if state.store.tick_workflow_run(&id).await.is_err() {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    // Wake the scheduler so freshly-created step tasks get assigned promptly.
    state.assignment_notify.notify_waiters();
    match state.store.get_workflow_run_with_steps(&id).await {
        Ok(Some((r, steps))) => Ok(Json(WorkflowRunWithSteps { run: r, steps })),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub async fn workflow_run_plan(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<WorkflowProjection>, StatusCode> {
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

pub async fn approve_workflow_plan_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<WorkflowRunWithSteps>, StatusCode> {
    match state.store.approve_workflow_plan(&id).await {
        Ok(()) => {
            // Wake the scheduler so the freshly-expanded steps assign.
            state.assignment_notify.notify_waiters();
            // Plan 536: single store call for the fresh run + steps.
            match state.store.get_workflow_run_with_steps(&id).await {
                Ok(Some((r, steps))) => Ok(Json(WorkflowRunWithSteps { run: r, steps })),
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

pub async fn cancel_workflow_run_handler(
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
