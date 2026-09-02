//! CP-managed adapter environment: point adapters at non-default endpoints
//! (custom Claude base URL + token, alternative opencode providers, …).
//!
//!   GET    /v1/adapter-env        — list entries
//!   PUT    /v1/adapter-env        — upsert {adapter|"*", key, value, node_id?}
//!   DELETE /v1/adapter-env/{id}   — remove

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;

use crate::AppState;

#[derive(Deserialize)]
pub struct SetAdapterEnvRequest {
    /// Adapter id (`claude`, `opencode`, …) or `*` for all adapters.
    pub adapter: String,
    /// Env var name, e.g. ANTHROPIC_BASE_URL.
    pub key: String,
    pub value: String,
    pub node_id: Option<String>,
}

pub async fn list_adapter_env(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.store.list_adapter_env().await {
        Ok(rows) => (StatusCode::OK, Json(serde_json::json!({ "items": rows }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

pub async fn set_adapter_env(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SetAdapterEnvRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key_ok = !req.key.is_empty()
        && req
            .key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        && req
            .key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase());
    if !key_ok || req.adapter.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "bad adapter or key" })),
        );
    }
    match state
        .store
        .upsert_adapter_env(&req.adapter, &req.key, &req.value, req.node_id.as_deref())
        .await
    {
        Ok(id) => (StatusCode::OK, Json(serde_json::json!({ "id": id }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

pub async fn remove_adapter_env(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.store.remove_adapter_env(id).await {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such entry" })),
        ),
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({ "removed": id }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}
