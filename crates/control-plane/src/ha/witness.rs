//! Witness mode: a voter with no database. It persists term/vote in
//! `AGENTGRID_HA_WITNESS_DIR` and answers only health + HA peer RPC.

use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use super::protocol::{HeartbeatRequest, HeartbeatResponse, VoteRequest, VoteResponse};
use super::role::HaRuntime;
use super::{election, routes, state_file};

/// Load the durable vote/term before the listener starts. Called once at boot.
pub fn start(rt: &HaRuntime) -> Result<()> {
    std::fs::create_dir_all(&rt.config.witness_dir)?;
    let path = rt.config.witness_dir.join("ha-witness-state.json");
    let persisted = state_file::load(&path);
    rt.shared.set_vote_path(path);
    rt.shared
        .set_term_and_vote(persisted.term, persisted.voted_for.as_deref())?;
    tracing::info!(
        node = %rt.config.node_id,
        term = rt.shared.term(),
        dir = %rt.config.witness_dir.display(),
        "HA witness starting"
    );
    Ok(())
}

pub fn router(rt: Arc<HaRuntime>) -> Router {
    Router::new()
        .route("/health/live", get(health))
        .route("/health/ready", get(health))
        .route("/v1/ha/status", get(status))
        .route("/v1/ha/vote", post(vote))
        .route("/v1/ha/heartbeat", post(heartbeat))
        .layer(axum::middleware::from_fn_with_state(
            rt.clone(),
            require_token,
        ))
        .with_state(rt)
}

async fn health() -> StatusCode {
    StatusCode::OK
}

async fn require_token(State(rt): State<Arc<HaRuntime>>, req: Request, next: Next) -> Response {
    if !routes::token_ok(&rt, req.headers()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(req).await
}

async fn status(State(rt): State<Arc<HaRuntime>>) -> Response {
    match election::status_of(&rt, None).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => {
            tracing::error!("witness status failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn vote(
    State(rt): State<Arc<HaRuntime>>,
    Json(req): Json<VoteRequest>,
) -> Json<VoteResponse> {
    Json(election::handle_vote(&rt, None, 0, req).await)
}

async fn heartbeat(
    State(rt): State<Arc<HaRuntime>>,
    Json(req): Json<HeartbeatRequest>,
) -> Json<HeartbeatResponse> {
    Json(election::handle_heartbeat(&rt, None, 0, req).await)
}
