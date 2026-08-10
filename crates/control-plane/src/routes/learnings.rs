//! Plan 2.8 (#19): repo-learnings routes.
//!
//! Repository learnings are short factual statements that get injected into
//! future attempt prompts. Admin/user manages them via:
//!   POST   /v1/repos/{repo}/learnings        — add (pending approval)
//!   GET    /v1/repos/{repo}/learnings         — list (filter by approved)
//!   POST   /v1/learnings/{id}/approve        — flip approved → 1
//!   DELETE /v1/learnings/{id}                 — remove
//!
//! Scheduler pulls the top-N approved rows via `top_approved_for_repo` and
//! prepends them to the attempt prompt (see store/scheduler.rs).

use std::sync::Arc;

use agentgrid_common::{AddLearningRequest, RepoLearning};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::AppState;

#[derive(Deserialize)]
pub struct ListLearningsQuery {
    #[serde(default)]
    pub approved: Option<bool>,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    100
}

/// `POST /v1/repos/{repo}/learnings` — insert a new learning (always
/// starts as unapproved; a human must approve before it influences any
/// attempt prompt).
pub async fn add_learning(
    State(state): State<Arc<AppState>>,
    Path(repository): Path<String>,
    Json(body): Json<AddLearningRequest>,
) -> Result<(StatusCode, Json<RepoLearning>), StatusCode> {
    if body.repository != repository {
        return Err(StatusCode::BAD_REQUEST);
    }
    let id = state
        .store
        .add_learning(
            &repository,
            &body.statement,
            body.confidence,
            body.source_attempt_id.as_deref(),
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rows = state
        .store
        .list_learnings(&repository, false, 1)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let row = rows
        .into_iter()
        .next()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((
        StatusCode::CREATED,
        Json(RepoLearning {
            id,
            repository,
            statement: row.statement,
            confidence: row.confidence,
            source_attempt_id: row.source_attempt_id,
            approved: row.approved,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }),
    ))
}

/// `GET /v1/repos/{repo}/learnings` — list. `?approved=true|false` filters,
/// otherwise returns all newest-first.
pub async fn list_learnings(
    State(state): State<Arc<AppState>>,
    Path(repository): Path<String>,
    Query(q): Query<ListLearningsQuery>,
) -> Result<Json<Vec<RepoLearning>>, StatusCode> {
    let rows = state
        .store
        .list_learnings(&repository, q.approved.unwrap_or(false), q.limit)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // When ?approved is not given at all, list_learnings(false) returns all
    // but ordered. For the simple case we post-filter when approved is Some.
    let filtered: Vec<RepoLearning> = rows
        .into_iter()
        .filter(|r| q.approved.is_none() || Some(r.approved) == q.approved)
        .map(|r| RepoLearning {
            id: r.id,
            repository: r.repository,
            statement: r.statement,
            confidence: r.confidence,
            source_attempt_id: r.source_attempt_id,
            approved: r.approved,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect();
    Ok(Json(filtered))
}

/// `POST /v1/learnings/{id}/approve` — flip `approved` to 1. The body is
/// optional; an empty POST means approve, `{"approved": false}` means
/// demote back to pending.
#[derive(Deserialize)]
pub struct ApproveBody {
    #[serde(default = "default_true")]
    pub approved: bool,
}
fn default_true() -> bool {
    true
}

pub async fn approve_learning(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<ApproveBody>>,
) -> Result<StatusCode, StatusCode> {
    let target = body.map(|b| b.0.approved).unwrap_or(true);
    let ok = state
        .store
        .approve_learning(&id, target)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if ok {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

/// `DELETE /v1/learnings/{id}` — remove a learning.
pub async fn delete_learning(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let ok = state
        .store
        .delete_learning(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if ok {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}
