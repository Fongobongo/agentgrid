//! CP-managed egress proxies: operators register proxy URLs (global pool or
//! per-node), and nodes receive their effective list in `PollResponse`.
//!
//!   GET    /v1/proxies          — list (admin)
//!   POST   /v1/proxies          — add {url, node_id?}
//!   DELETE /v1/proxies/{id}     — remove

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::AppState;

#[derive(Deserialize)]
pub struct AddProxyRequest {
    pub url: String,
    pub node_id: Option<String>,
}

pub async fn list_proxies(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.store.list_proxies().await {
        Ok(rows) => (StatusCode::OK, Json(serde_json::json!({ "items": rows }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

pub async fn add_proxy(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AddProxyRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Cheap sanity: <scheme>://<host>[:port], http(s)/socks5 only.
    let ok = req
        .url
        .split_once("://")
        .map(|(scheme, rest)| {
            matches!(scheme, "http" | "https" | "socks5")
                && rest
                    .split(['/', '?', '#'])
                    .next()
                    .and_then(|auth_host| auth_host.rsplit('@').next())
                    .is_some_and(|h| !h.trim_start_matches('[').is_empty())
        })
        .unwrap_or(false);
    if !ok {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid proxy url" })),
        );
    }
    match state
        .store
        .add_proxy(&req.url, req.node_id.as_deref())
        .await
    {
        Ok(id) => (StatusCode::CREATED, Json(serde_json::json!({ "id": id }))),
        Err(e) if e.to_string().contains("UNIQUE") => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "proxy already registered" })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

pub async fn remove_proxy(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.store.remove_proxy(id).await {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such proxy" })),
        ),
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({ "removed": id }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}
