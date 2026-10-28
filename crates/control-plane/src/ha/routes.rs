//! HA HTTP surface: peer RPC (`/v1/ha/*`) and the standby write gate.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio_util::io::ReaderStream;

use super::protocol::{
    HeartbeatRequest, HeartbeatResponse, PromoteResponse, VoteRequest, VoteResponse,
    ARTIFACT_ATTEMPT_HEADER, ARTIFACT_NAME_HEADER, ARTIFACT_SHA_HEADER, HA_TOKEN_HEADER,
};
use super::role::HaRuntime;
use super::{election, snapshot};
use crate::AppState;

pub fn data_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/ha/status", get(status))
        .route("/v1/ha/vote", post(vote))
        .route("/v1/ha/heartbeat", post(heartbeat))
        .route("/v1/ha/promote", post(promote))
        .route("/v1/ha/demote", post(demote))
        .route("/v1/ha/snapshot", post(snapshot_now))
        .route("/v1/ha/snapshot/{id}", get(snapshot_download))
        .route("/v1/ha/snapshot/{id}/manifest", get(snapshot_manifest))
        .route("/v1/ha/artifact", get(artifact_get).put(artifact_put))
        .layer(axum::middleware::from_fn_with_state(state, data_token))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn token_ok(rt: &HaRuntime, headers: &HeaderMap) -> bool {
    let Some(v) = headers.get(HA_TOKEN_HEADER).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    constant_time_eq(v.as_bytes(), rt.config.cluster_token.as_bytes())
}

async fn data_token(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if !state.ha.is_ha() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !token_ok(&state.ha, req.headers()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(req).await
}

async fn status(State(state): State<Arc<AppState>>) -> Response {
    match election::status_of(&state.ha, Some(&state.store)).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => error_500(e),
    }
}

async fn vote(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VoteRequest>,
) -> Json<VoteResponse> {
    let applied = state.ha.shared.applied_seq();
    Json(election::handle_vote(&state.ha, Some(&state), applied, req).await)
}

async fn heartbeat(
    State(state): State<Arc<AppState>>,
    Json(req): Json<HeartbeatRequest>,
) -> Json<HeartbeatResponse> {
    let applied = state.ha.shared.applied_seq();
    Json(election::handle_heartbeat(&state.ha, Some(&state), applied, req).await)
}

async fn promote(State(state): State<Arc<AppState>>) -> Response {
    match election::manual_promote(&state).await {
        Ok(()) => Json(PromoteResponse {
            ok: true,
            role: state.ha.role().as_str().into(),
            epoch: state.ha.gate.epoch(),
            message: "promoted".into(),
        })
        .into_response(),
        Err(e) => error_500(e),
    }
}

async fn demote(State(state): State<Arc<AppState>>) -> Response {
    election::demote(&state, "manual demote").await;
    Json(PromoteResponse {
        ok: true,
        role: state.ha.role().as_str().into(),
        epoch: state.ha.gate.epoch(),
        message: "demoted".into(),
    })
    .into_response()
}

async fn snapshot_now(State(state): State<Arc<AppState>>) -> Response {
    if !state.ha.is_leader() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let rt = &state.ha;
    match snapshot::create(
        &state.store,
        &rt.shared,
        &rt.config.cluster_id,
        rt.gate.epoch(),
        rt.shared.term(),
    )
    .await
    {
        Ok(m) => Json(m).into_response(),
        Err(e) => error_500(e),
    }
}

async fn snapshot_manifest(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if !state.ha.is_leader() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    match snapshot::read_manifest(state.store.data_dir(), &id) {
        Ok(m) => Json(m).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn snapshot_download(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if !state.ha.is_leader() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let path = match snapshot::snapshot_path(state.store.data_dir(), &id) {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let stream = ReaderStream::new(file);
    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn artifact_headers(headers: &HeaderMap) -> Option<(String, String)> {
    let attempt = headers.get(ARTIFACT_ATTEMPT_HEADER)?.to_str().ok()?;
    let name = headers.get(ARTIFACT_NAME_HEADER)?.to_str().ok()?;
    if !crate::store::is_safe_opaque_id(attempt) || !crate::store::is_safe_artifact_name(name) {
        return None;
    }
    Some((attempt.to_string(), name.to_string()))
}

/// Peer artifact read: raw stored bytes (`.zst` frame included) for the
/// on-demand pull path after promotion or for a replica that joined late.
async fn artifact_get(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some((attempt, stored_name)) = artifact_headers(&headers) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match state
        .store
        .artifact_blob_bytes(&attempt, &stored_name)
        .await
    {
        Ok(Some(bytes)) => Response::builder()
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(ARTIFACT_SHA_HEADER, agentgrid_common::sha256_hex(&bytes))
            .body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::warn!("HA artifact read failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Peer artifact push: the leader eagerly copies each upload to the data
/// replicas. Metadata arrives with the database snapshot; only bytes move here.
async fn artifact_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some((attempt, stored_name)) = artifact_headers(&headers) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(sha) = headers
        .get(ARTIFACT_SHA_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if agentgrid_common::sha256_hex(&body) != sha.trim().to_ascii_lowercase() {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    match state
        .store
        .write_artifact_blob(&attempt, &stored_name, &body, &sha)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::warn!("HA artifact write failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn error_500(e: anyhow::Error) -> Response {
    tracing::error!("HA endpoint failed: {e}");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// Standby write gate (outermost data-router layer). Health, HA peer RPC,
/// version and metrics stay reachable; every other route gets a 503 that
/// tells clients which node to follow.
pub async fn leader_gate(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if !state.ha.is_ha() || state.ha.is_leader() {
        return next.run(req).await;
    }
    let path = req.uri().path();
    if path.starts_with("/health")
        || path.starts_with("/v1/ha/")
        || path == "/v1/version"
        || path == "/metrics"
    {
        return next.run(req).await;
    }
    let mut res = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": {
                "code": "not_leader",
                "message": "this control-plane replica is not the leader",
            }
        })),
    )
        .into_response();
    let headers = res.headers_mut();
    let _ = headers.try_insert(
        header::HeaderName::from_static("x-agentgrid-role"),
        state
            .ha
            .role()
            .as_str()
            .try_into()
            .unwrap_or(header::HeaderValue::from_static("standby")),
    );
    let _ = headers.try_insert(
        header::HeaderName::from_static("x-agentgrid-epoch"),
        state
            .ha
            .gate
            .epoch()
            .to_string()
            .try_into()
            .unwrap_or(header::HeaderValue::from_static("0")),
    );
    let _ = headers.try_insert(
        header::HeaderName::from_static("retry-after"),
        header::HeaderValue::from_static("2"),
    );
    if let Some(leader) = state.ha.shared.leader() {
        if let Ok(v) = leader.url.try_into() {
            let _ = res
                .headers_mut()
                .try_insert(header::HeaderName::from_static("x-agentgrid-leader"), v);
        }
    }
    res
}
