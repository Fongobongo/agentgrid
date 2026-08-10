//! Plan 1.12 (#7): shared context / memory between parallel attempts of a
//! task group. Flat scoped notes: set/get/list/delete by (group, key).

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};

use crate::AppState;

/// `PUT /v1/task-groups/{id}/context/{key}` — set (upsert) one note.
pub async fn set_context(
    State(state): State<Arc<AppState>>,
    Path((group_id, key)): Path<(String, String)>,
    Json(body): Json<SetContextBody>,
) -> Result<StatusCode, StatusCode> {
    state
        .store
        .set_shared_context(&group_id, &key, &body.value)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/task-groups/{id}/context` — list all notes for the group.
pub async fn list_context(
    State(state): State<Arc<AppState>>,
    Path(group_id): Path<String>,
) -> Result<Json<Vec<agentgrid_common::SharedContextEntry>>, StatusCode> {
    state
        .store
        .list_shared_context(&group_id)
        .await
        .map(Json)
        .map_err(|_| StatusCode::BAD_REQUEST)
}

/// `GET /v1/task-groups/{id}/context/{key}` — read one note's value. 404 when
/// absent. Returns the raw value string (not a wrapper) for easy CLI/SDK use.
pub async fn get_context(
    State(state): State<Arc<AppState>>,
    Path((group_id, key)): Path<(String, String)>,
) -> Result<Json<String>, StatusCode> {
    let Some(value) = state
        .store
        .get_shared_context(&group_id, &key)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?
    else {
        return Err(StatusCode::NOT_FOUND);
    };
    Ok(Json(value))
}

/// `DELETE /v1/task-groups/{id}/context/{key}` — delete one note.
pub async fn delete_context(
    State(state): State<Arc<AppState>>,
    Path((group_id, key)): Path<(String, String)>,
) -> Result<StatusCode, StatusCode> {
    state
        .store
        .delete_shared_context(&group_id, &key)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
pub struct SetContextBody {
    pub value: String,
}
