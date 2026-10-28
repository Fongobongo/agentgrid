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
    HeartbeatRequest, HeartbeatResponse, PromoteResponse, SecretResponse, VoteRequest,
    VoteResponse, ARTIFACT_ATTEMPT_HEADER, ARTIFACT_NAME_HEADER, ARTIFACT_SHA_HEADER,
    HA_APPLIED_SEQ_HEADER, HA_EPOCH_HEADER, HA_JWT_FINGERPRINT_HEADER, HA_REASON_HEADER,
    HA_SCHEMA_HEADER, HA_SEQ_FIRST_HEADER, HA_SEQ_LAST_HEADER, HA_TOKEN_HEADER,
};
use super::role::HaRuntime;
use super::{election, snapshot};
use crate::store::ApplyOutcome;
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
        .route("/v1/ha/changeset", post(changeset))
        .route("/v1/ha/artifact", get(artifact_get).put(artifact_put))
        .route("/v1/ha/secret", get(secret))
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
    // Secret provisioning is the one call a not-yet-provisioned host makes; it
    // cannot present a fingerprint it does not have yet. It still needs the
    // cluster token, leader role, TLS config and an explicit opt-in flag.
    if req.uri().path() != "/v1/ha/secret" {
        if let Some(expected) = state.ha.jwt_fingerprint() {
            // A witness has no JWT secret and therefore sends no fingerprint;
            // it still authenticates with the cluster token. A data host that
            // presents a *different* fingerprint is misconfigured and is
            // refused before it can vote or acknowledge writes.
            if let Some(presented) = req
                .headers()
                .get(HA_JWT_FINGERPRINT_HEADER)
                .and_then(|v| v.to_str().ok())
            {
                if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
            }
        }
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

/// Standby side of incremental replication: apply one changeset batch. Any
/// reason to refuse answers `409` with the local applied sequence so the
/// leader can cut a snapshot instead.
async fn changeset(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let rt = &state.ha;
    let number = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
    };
    let schema = headers
        .get(HA_SCHEMA_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    let (Some(epoch), Some(first_seq), Some(last_seq), Some(schema_version)) = (
        number(HA_EPOCH_HEADER),
        number(HA_SEQ_FIRST_HEADER),
        number(HA_SEQ_LAST_HEADER),
        schema,
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let reject = |applied: u64, reason: &str| changeset_rejected(applied, reason);
    if rt.is_leader() {
        return reject(rt.shared.applied_seq(), "this node is the leader");
    }
    if rt.shared.need_full_snapshot() {
        return reject(rt.shared.applied_seq(), "needs full snapshot");
    }
    let local_epoch = rt.gate.epoch();
    if epoch < local_epoch {
        return reject(rt.shared.applied_seq(), "stale epoch");
    }
    if epoch > local_epoch {
        rt.gate.set_epoch(epoch);
    }
    let _apply = rt.shared.apply_lock.lock().await;
    if rt.shared.need_full_snapshot() {
        return reject(rt.shared.applied_seq(), "needs full snapshot");
    }
    match state
        .store
        .apply_changeset_batch(first_seq, last_seq, schema_version, &body)
        .await
    {
        Ok(ApplyOutcome::Applied { seq }) => {
            rt.shared.set_applied_seq(seq);
            rt.shared.touch_leader();
            super::incremental::note_batches(1);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(ApplyOutcome::Duplicate) => {
            if rt.shared.applied_seq() < last_seq {
                rt.shared.set_applied_seq(last_seq);
            }
            rt.shared.touch_leader();
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(ApplyOutcome::Rejected {
            applied_seq,
            reason,
        }) => {
            super::incremental::note_apply_failure();
            reject(applied_seq, &reason)
        }
        Err(e) => {
            tracing::warn!("changeset apply failed: {e}");
            super::incremental::note_apply_failure();
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn changeset_rejected(applied_seq: u64, reason: &str) -> Response {
    let reason: String = reason
        .chars()
        .filter(|c| !c.is_control())
        .take(160)
        .collect();
    let mut res = StatusCode::CONFLICT.into_response();
    let headers = res.headers_mut();
    let _ = headers.try_insert(
        header::HeaderName::from_static(HA_APPLIED_SEQ_HEADER),
        header::HeaderValue::from_str(&applied_seq.to_string())
            .unwrap_or(header::HeaderValue::from_static("0")),
    );
    let _ = headers.try_insert(
        header::HeaderName::from_static(HA_REASON_HEADER),
        header::HeaderValue::from_str(&reason)
            .unwrap_or(header::HeaderValue::from_static("rejected")),
    );
    res
}

async fn snapshot_now(State(state): State<Arc<AppState>>) -> Response {
    if !state.ha.is_leader() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let rt = &state.ha;
    let _create = rt.shared.snapshot_lock.lock().await;
    match snapshot::create(rt, &state.store).await {
        Ok(m) => Json(m).into_response(),
        Err(e) => error_500(e),
    }
}

/// Opt-in one-time JWT secret provisioning. Requires the cluster token, leader
/// role, TLS (unless plaintext was explicitly allowed) and
/// `AGENTGRID_HA_SECRET_FETCH=1`. Disable the flag after provisioning.
async fn secret(State(state): State<Arc<AppState>>) -> Response {
    let rt = &state.ha;
    if !rt.config.secret_fetch {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !rt.is_leader() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if !rt.config.allow_plaintext && !rt.config.advertise_url.starts_with("https://") {
        return StatusCode::FORBIDDEN.into_response();
    }
    match (rt.jwt_secret(), rt.jwt_fingerprint()) {
        (Some(secret), Some(fingerprint)) => Json(SecretResponse {
            secret,
            fingerprint,
        })
        .into_response(),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
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
