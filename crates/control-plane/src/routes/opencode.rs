//! opencode-config profile routes (feature "opencode profiles").
//!
//! OPERATOR (JWT) routes:
//!   GET    /v1/opencode-profiles                    — list all profiles
//!   PUT    /v1/opencode-profiles/{name}             — create-or-replace
//!   DELETE /v1/opencode-profiles/{name}             — remove
//!   POST   /v1/nodes/{id}/opencode-profile          — assign/clear profile
//!   GET    /v1/nodes/{id}/opencode-audit            — recent apply events
//!
//! NODE (Bearer node-credential) route:
//!   GET    /v1/opencode-config/active               — the node's current profile
//!
//! Apply flow: profile change → CP pushes `NodeWsMsg::ConfigUpdate` to
//! subscribed nodes → node pulls this route when the pushed hash differs →
//! node writes `~/.config/opencode/opencode.json` atomically.

use std::sync::Arc;

use agentgrid_common::ws::NodeWsMsg;
use agentgrid_common::{
    ActiveOpencodeConfigResponse, AssignOpencodeProfileRequest, ListResponse,
    OpencodeConfigAuditEntry, OpencodeProfile, UpsertOpencodeProfileRequest,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::auth::AuthedNode;
use crate::AppState;
use axum::Extension;

/// `GET /v1/opencode-profiles` — list every stored profile.
pub async fn list_profiles(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ListResponse<OpencodeProfile>>, StatusCode> {
    match state.store.list_opencode_profiles().await {
        Ok(items) => Ok(Json(ListResponse::new(items, None))),
        Err(e) => {
            tracing::error!("list_opencode_profiles: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// `GET /v1/opencode-profiles/{name}` — show one profile.
pub async fn get_profile(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<OpencodeProfile>, StatusCode> {
    state
        .store
        .get_opencode_profile(&name)
        .await
        .map_err(|e| {
            tracing::error!("get_opencode_profile: {e}");
            StatusCode::SERVICE_UNAVAILABLE
        })?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// `PUT /v1/opencode-profiles/{name}` — create-or-replace. On successful
/// write the CP pushes ConfigUpdate to every node currently subscribed.
pub async fn upsert_profile(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<UpsertOpencodeProfileRequest>,
) -> Result<Json<OpencodeProfile>, StatusCode> {
    let profile = state
        .store
        .upsert_opencode_profile(&name, body.config)
        .await
        .map_err(|e| {
            tracing::warn!("opencode upsert validation failed: {e}");
            StatusCode::BAD_REQUEST
        })?;

    let nodes = state
        .store
        .list_nodes_for_profile(&profile.id)
        .await
        .unwrap_or_default();
    push_config_update(
        &state,
        &nodes,
        Some(profile.id.clone()),
        Some(profile.hash.clone()),
    )
    .await;
    Ok(Json(profile))
}

/// `DELETE /v1/opencode-profiles/{name}` — remove the profile. Nodes keep
/// their last-applied on-disk config (harmless); the FK pointer is cleared
/// via ON DELETE SET NULL.
pub async fn delete_profile(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<StatusCode, StatusCode> {
    // Snapshot the affected nodes BEFORE the delete so the push can wake
    // them into dropping the profile voluntarily.
    let affected: Vec<String> = match state.store.get_opencode_profile(&name).await {
        Ok(Some(p)) => state
            .store
            .list_nodes_for_profile(&p.id)
            .await
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    match state.store.delete_opencode_profile(&name).await {
        Ok(true) => {
            push_config_update(&state, &affected, None, None).await;
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(false) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("delete_opencode_profile: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// `POST /v1/nodes/{id}/opencode-profile` — assign (`profile_id: "…"`) or
/// clear (`null`) the node's profile. Pushes ConfigUpdate to that node so
/// it applies immediately when connected.
pub async fn assign_node_profile(
    State(state): State<Arc<AppState>>,
    Path(node_id): Path<String>,
    Json(body): Json<AssignOpencodeProfileRequest>,
) -> Result<StatusCode, StatusCode> {
    if let Some(pid) = &body.profile_id {
        // Confirm the profile exists — a dangling FK is a confusing
        // operator experience. Cheapest is a store query on the id column
        // (no hash bump, no allocation).
        let exists = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM opencode_profiles WHERE id = ?)",
        )
        .bind(pid)
        .fetch_one(&state.store.pool)
        .await
        .unwrap_or(0);
        if exists == 0 {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    let (applied, profile_id, hash) = state
        .store
        .assign_opencode_profile(&node_id, body.profile_id.as_deref())
        .await
        .map_err(|e| {
            tracing::error!("assign_opencode_profile: {e}");
            StatusCode::SERVICE_UNAVAILABLE
        })?;
    if !applied {
        return Err(StatusCode::NOT_FOUND);
    }
    push_config_update(&state, &[node_id], profile_id, hash).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/nodes/{id}/opencode-audit` — the last N apply events for a node.
#[derive(Deserialize)]
pub struct AuditQuery {
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    50
}

pub async fn list_audit(
    State(state): State<Arc<AppState>>,
    Path(node_id): Path<String>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<ListResponse<OpencodeConfigAuditEntry>>, StatusCode> {
    match state
        .store
        .list_opencode_audit(&node_id, q.limit.min(500))
        .await
    {
        Ok(items) => Ok(Json(ListResponse::new(items, None))),
        Err(e) => {
            tracing::error!("list_opencode_audit: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// `GET /v1/opencode-config/active` — node pulls its assigned profile. Sits
/// behind `AuthedNode` (Bearer node credential) so a non-node caller gets
/// 401.
pub async fn get_active_config(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
) -> Result<Json<ActiveOpencodeConfigResponse>, StatusCode> {
    let profile = state
        .store
        .node_opencode_profile(&auth.node_id)
        .await
        .map_err(|e| {
            tracing::error!("node_opencode_profile: {e}");
            StatusCode::SERVICE_UNAVAILABLE
        })?;
    let resp = match profile {
        Some(p) => ActiveOpencodeConfigResponse {
            profile_id: Some(p.id),
            hash: Some(p.hash),
            config: Some(p.config),
        },
        None => ActiveOpencodeConfigResponse {
            profile_id: None,
            hash: None,
            config: None,
        },
    };
    Ok(Json(resp))
}

/// Internal: multicast `NodeWsMsg::ConfigUpdate` to a list of node ids.
/// This is also the integration point for the poll fallback: when the node
/// heartbeats next it will re-read its profile (poll nodes pick the change
/// up at the next long-poll cycle).
async fn push_config_update(
    state: &AppState,
    node_ids: &[String],
    profile_id: Option<String>,
    hash: Option<String>,
) {
    for node_id in node_ids {
        state
            .ws_registry
            .send(
                node_id,
                &NodeWsMsg::ConfigUpdate {
                    profile_id: profile_id.clone(),
                    hash: hash.clone(),
                },
            )
            .await;
    }
}
