//! Node attempt routes: cancel check, event ingest, complete, ack, validate,
//! agent-session creation.

use std::sync::Arc;

use agentgrid_common::{
    CancelState, CompleteAttemptRequest, CreateAgentSessionRequest, IngestEventsRequest,
};
use axum::{
    extract::{Extension, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use crate::auth::{check_attempt_owner, check_fencing_token, fencing_token_header, AuthedNode};
use crate::middleware;
use crate::AppState;

pub async fn attempt_cancel_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
) -> Response {
    if let Err(code) = check_attempt_owner(&state, &auth, &attempt_id).await {
        return code.into_response();
    }
    // No fencing check here: this is a read of cancel_requested (a polling
    // endpoint), not a mutation.
    let requested = state
        .store
        .attempt_cancel_requested(&attempt_id)
        .await
        .unwrap_or(false);
    Json(CancelState {
        cancel_requested: requested,
    })
    .into_response()
}

pub async fn ingest_events(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<IngestEventsRequest>,
) -> Response {
    if let Err(code) = check_attempt_owner(&state, &auth, &attempt_id).await {
        return code.into_response();
    }
    if let Err(code) = check_fencing_token(
        &state,
        &attempt_id,
        fencing_token_header(&headers).as_deref(),
    )
    .await
    {
        return code.into_response();
    }
    for e in &req.events {
        if e.payload.to_string().len() > state.limits.event {
            state
                .event_rejections
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
    }
    // Hardening P1 item 14: per-node rate limit so a single node cannot flood
    // the control plane with event batches. Over the limit → 429, with a
    // counted rejection so it shows in metrics.
    {
        let now = chrono::Utc::now().timestamp();
        let allowed = state.event_rate.lock().await.admit(&auth.node_id, now);
        if !allowed {
            state
                .event_rejections
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    }
    // Hardening P1 item 14: bound the batch itself — event count and the
    // summed payload bytes — so one request cannot flood the store.
    if req.events.len() > state.limits.event_batch_count {
        state
            .event_rejections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let total: usize = req.events.iter().map(|e| e.payload.to_string().len()).sum();
    if total > state.limits.event_batch_bytes {
        state
            .event_rejections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    match state.store.ingest_events(&attempt_id, &req).await {
        Ok(ack) if ack.accepted > 0 || ack.highest_contiguous_sequence.is_some() => {
            // Hardening P1 item 14: a batch whose max sequence exceeds the
            // contiguous prefix (highest_contiguous_sequence) introduced a
            // gap — out-of-order or skipped-sequence delivery. Count it once
            // per such batch for observability; the durable outbox still
            // redrives the missing sequences.
            let max_seq = req.events.iter().map(|e| e.sequence).max();
            if let (Some(max_seq), Some(prefix)) = (max_seq, ack.highest_contiguous_sequence) {
                if max_seq > prefix {
                    state
                        .event_gaps
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            (StatusCode::OK, Json(ack)).into_response()
        }
        // Either the attempt is gone or it is terminal — both are
        // rejections worth surfacing in observability.
        Ok(_) => {
            state
                .event_rejections
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            StatusCode::NOT_FOUND.into_response()
        }
        Err(e) => {
            tracing::error!("ingest_events failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn complete_attempt(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<CompleteAttemptRequest>,
) -> Response {
    if let Err(code) = check_attempt_owner(&state, &auth, &attempt_id).await {
        return code.into_response();
    }
    if let Err(code) = check_fencing_token(
        &state,
        &attempt_id,
        fencing_token_header(&headers).as_deref(),
    )
    .await
    {
        return code.into_response();
    }
    match state.store.complete_attempt(&attempt_id, &req).await {
        Ok(true) => StatusCode::OK.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            // Hardening P1 item 13: map invalid state transition to 409 Conflict
            // with a machine-readable code (hardening P2 item 19). The `?`
            // operator wraps the raw `InvalidTransition` in anyhow (via its
            // blanket From), NOT the StoreTransitionError marker — so check
            // both shapes.
            if e.downcast_ref::<crate::store::StoreTransitionError>()
                .is_some()
                || e.downcast_ref::<agentgrid_common::InvalidTransition>()
                    .is_some()
            {
                return middleware::api_error(
                    StatusCode::CONFLICT,
                    "invalid_state_transition",
                    "attempt cannot transition from its current state",
                );
            }
            tracing::error!("complete_attempt failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn ack_attempt_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
    headers: HeaderMap,
) -> StatusCode {
    if let Err(code) = check_attempt_owner(&state, &auth, &attempt_id).await {
        return code;
    }
    if let Err(code) = check_fencing_token(
        &state,
        &attempt_id,
        fencing_token_header(&headers).as_deref(),
    )
    .await
    {
        return code;
    }
    match state.store.ack_attempt(&attempt_id).await {
        Ok(true) => StatusCode::OK,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("ack_attempt failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Hardening P0 item 12: the node signals it has started the post-agent
/// validation command; the CP moves the attempt (and task) `running →
/// validating`. Ownership + fencing checked like every other node mutation.
pub async fn begin_validate_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
    headers: HeaderMap,
) -> StatusCode {
    if let Err(code) = check_attempt_owner(&state, &auth, &attempt_id).await {
        return code;
    }
    if let Err(code) = check_fencing_token(
        &state,
        &attempt_id,
        fencing_token_header(&headers).as_deref(),
    )
    .await
    {
        return code;
    }
    match state.store.begin_validate(&attempt_id).await {
        Ok(true) => StatusCode::OK,
        // Not `running` (already validating, terminal, or gone): idempotent
        // no-op so a retried validation signal never 500s.
        Ok(false) => StatusCode::OK,
        Err(e) => {
            tracing::error!("begin_validate failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

pub async fn create_agent_session_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<CreateAgentSessionRequest>,
) -> Response {
    if let Err(code) = check_attempt_owner(&state, &auth, &attempt_id).await {
        return code.into_response();
    }
    if let Err(code) = check_fencing_token(
        &state,
        &attempt_id,
        fencing_token_header(&headers).as_deref(),
    )
    .await
    {
        return code.into_response();
    }
    match state
        .store
        .create_agent_session(&attempt_id, &req.adapter)
        .await
    {
        Ok(id) => (
            StatusCode::OK,
            Json(serde_json::json!({ "session_id": id })),
        )
            .into_response(),
        Err(e) => {
            // Hardening P2 item 19: log the full internal error chain here, but
            // never send it to the client — return an opaque 500 body.
            tracing::error!("create_agent_session failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal error"})),
            )
                .into_response()
        }
    }
}
