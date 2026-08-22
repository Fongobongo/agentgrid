//! Approval routes: list/answer/create/get.

use std::sync::Arc;

use agentgrid_common::{ApprovalEvent, ApprovalView, ListResponse};
use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::auth::AuthedUser;
use crate::AppState;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ApprovalListQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    after_created_at: Option<String>,
    #[serde(default)]
    after_id: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

pub async fn list_approvals_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ApprovalListQuery>,
) -> Result<Json<ListResponse<ApprovalView>>, StatusCode> {
    let status = q
        .status
        .and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok());
    // Hardening P2 item 20: keyset cursor — only a complete pair is a cursor.
    let after = match (&q.after_created_at, &q.after_id) {
        (Some(c), Some(i)) if !c.is_empty() && !i.is_empty() => Some((c.clone(), i.clone())),
        _ => None,
    };
    match state.store.list_approvals(status, after, q.limit).await {
        Ok(items) => {
            let next_cursor = if items.len() == q.limit.unwrap_or(100).min(1000) as usize {
                items.last().map(|a| format!("{},{}", a.created_at, a.id))
            } else {
                None
            };
            Ok(Json(ListResponse::new(items, next_cursor)))
        }
        Err(e) => {
            tracing::error!("list_approvals failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

pub async fn allow_approval_handler(
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

pub async fn deny_approval_handler(
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
pub(crate) struct AnswerApprovalBody {
    /// Optional operator reason recorded with the decision (shown in the UI/CLI
    /// and audit). Omitted = default placeholder.
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateApprovalBody {
    attempt_id: String,
    session_id: Option<String>,
    permission: serde_json::Value,
    #[serde(default)]
    scope: Option<String>,
}

/// Stage 5: an ACP agent's `session/request_permission` creates a durable,
/// operator-answerable approval. Returns its id so the daemon can poll.
pub async fn create_approval_for_task_handler(
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

pub async fn get_approval_handler(
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
