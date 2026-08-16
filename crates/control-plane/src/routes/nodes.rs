//! Node routes: list/enroll/heartbeat/revoke/drain.

use std::sync::Arc;

use agentgrid_common::{
    ws::NodeWsMsg, EnrollRequest, EnrollResponse, EnrollTokenResponse, HeartbeatRequest,
    ListResponse, NodeView,
};
use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    Json,
};

use crate::auth::AuthedNode;
use crate::routes::WorkflowRunsQuery;
use crate::AppState;

pub async fn list_nodes(
    State(state): State<Arc<AppState>>,
    Query(q): Query<WorkflowRunsQuery>,
) -> Result<Json<ListResponse<NodeView>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — that
    // would read as "no nodes" to the client. Surface storage outage as 503.
    let after = match (&q.after_created_at, &q.after_id) {
        (Some(c), Some(i)) if !c.is_empty() && !i.is_empty() => Some((c.clone(), i.clone())),
        _ => None,
    };
    match state.store.list_nodes(after, q.limit).await {
        Ok(items) => {
            let next_cursor = if items.len() == q.limit.unwrap_or(100) as usize {
                items.last().map(|n| format!("{},{}", n.created_at, n.id))
            } else {
                None
            };
            Ok(Json(ListResponse::new(items, next_cursor)))
        }
        Err(e) => {
            tracing::error!("list_nodes failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// `GET /v1/nodes/{id}` — the control plane's view of one node. Read by
/// `ag node doctor` (report-only diagnostics). The route had shipped
/// DELETE-only, so the CLI's GET got a 405.
pub async fn get_node(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<crate::auth::AuthedUser>,
    Path(id): Path<String>,
) -> Result<Json<NodeView>, StatusCode> {
    match state.store.get_node(&id).await {
        Ok(Some(n)) => Ok(Json(n)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("get_node failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// Query for `GET /v1/audit` (plan 3.4): optional action filter + row cap.
#[derive(Debug, Default, serde::Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Newest-first audit trail (plan 3.4): who decided what, with an optional
/// action filter. Storage outage surfaces as 503, never an empty list.
pub async fn list_audit_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<ListResponse<crate::store::AuditEvent>>, StatusCode> {
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let action = q.action.as_deref().filter(|a| !a.is_empty());
    match state.store.list_audit(action, limit).await {
        Ok(items) => Ok(Json(ListResponse::new(items, None))),
        Err(e) => {
            tracing::error!("list_audit failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

pub async fn create_enrollment_token(
    State(state): State<Arc<AppState>>,
) -> Result<Json<EnrollTokenResponse>, StatusCode> {
    state
        .store
        .create_enrollment_token()
        .await
        .map(|(token, expires_at)| Json(EnrollTokenResponse { token, expires_at }))
        .map_err(|e| {
            tracing::error!("create_enrollment_token failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

pub async fn enroll(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EnrollRequest>,
) -> (StatusCode, Json<Option<EnrollResponse>>) {
    match state.store.enroll_node(&req).await {
        Ok(Some(r)) => {
            if agentgrid_common::is_incompatible_protocol(&req.protocol_version) {
                let _ = state.store.set_node_degraded(&r.node_id).await;
            }
            (StatusCode::OK, Json(Some(r)))
        }
        Ok(None) => (StatusCode::BAD_REQUEST, Json(None)),
        Err(e) => {
            tracing::error!("enroll failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(None))
        }
    }
}

pub async fn heartbeat(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Json(req): Json<HeartbeatRequest>,
) -> StatusCode {
    if agentgrid_common::is_incompatible_protocol(&req.protocol_version) {
        let _ = state.store.set_node_degraded(&auth.node_id).await;
    }
    match state.store.heartbeat(&auth.node_id, &req).await {
        Ok(true) => {
            // Plan 1.8 (#15): stash per-account usage for the usage endpoint
            // (memory-only; survives only while the CP runs).
            if !req.account_usage.is_empty() {
                state
                    .account_usage
                    .lock()
                    .await
                    .insert(auth.node_id.clone(), req.account_usage.clone());
            }
            // Stage 9.2: auto-fill the trust ledger from heartbeat discovery.
            // Upsert is idempotent and never overwrites an operator decision.
            let discovered: Vec<(String, String)> = req
                .discovered_skills
                .iter()
                .map(|s| (s.name.clone(), s.source.clone()))
                .collect();
            if !discovered.is_empty() {
                if let Err(e) = state.store.upsert_discovered_skills(&discovered).await {
                    // Discovery is best-effort; never fail the heartbeat on it.
                    tracing::warn!("skill discovery upsert failed: {e}");
                }
            }
            // Opencode-config drift auto-heal: the store side already wrote
            // the audit row; here we push a ConfigUpdate over the ws channel
            // so the node converges on the assigned hash.
            if let Some(applied) = &req.applied_opencode_hash {
                if let Ok(Some(profile_row)) =
                    state.store.node_opencode_profile(&auth.node_id).await
                {
                    if applied != &profile_row.hash {
                        state
                            .ws_registry
                            .send(
                                &auth.node_id,
                                &NodeWsMsg::ConfigUpdate {
                                    profile_id: Some(profile_row.id.clone()),
                                    hash: Some(profile_row.hash.clone()),
                                },
                            )
                            .await;
                        tracing::info!(
                            node_id = %auth.node_id,
                            profile_id = %profile_row.id,
                            "auto-heal push for opencode drift"
                        );
                    }
                }
            }
            StatusCode::OK
        }
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("heartbeat failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

pub async fn revoke_node(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<crate::auth::AuthedUser>>,
    Path(id): Path<String>,
) -> StatusCode {
    match state.store.revoke_node(&id).await {
        Ok(true) => {
            // Hardening P2 item 35: audit the security-sensitive mutation.
            let _ = state
                .store
                .audit(
                    "user",
                    auth.as_ref().map(|e| e.0.username.as_str()),
                    "node.revoke",
                    Some(&id),
                    None,
                )
                .await;
            StatusCode::OK
        }
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("revoke_node failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Hardening P2 item 37: drain (or undrain) a node for maintenance. A drained
/// node stops receiving NEW assignments while its in-flight attempts run to
/// completion; the heartbeat keeps it online.
#[derive(serde::Deserialize)]
pub(crate) struct NodeDrainQuery {
    #[serde(default)]
    drain: Option<bool>,
}

/// Plan 1.8 (#15): per-account usage for a node (reported via heartbeat).
/// In-memory; empty list when the node never reported usage.
pub async fn node_account_usage(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<agentgrid_common::AccountUsage>>, StatusCode> {
    Ok(Json(
        state
            .account_usage
            .lock()
            .await
            .get(&id)
            .cloned()
            .unwrap_or_default(),
    ))
}

pub async fn drain_node_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<crate::auth::AuthedUser>>,
    Path(id): Path<String>,
    Query(q): Query<NodeDrainQuery>,
) -> StatusCode {
    let drained = q.drain.unwrap_or(true);
    match state.store.set_node_drained(&id, drained).await {
        Ok(true) => {
            let _ = state
                .store
                .audit(
                    "user",
                    auth.as_ref().map(|e| e.0.username.as_str()),
                    if drained {
                        "node.drain"
                    } else {
                        "node.undrain"
                    },
                    Some(&id),
                    None,
                )
                .await;
            StatusCode::OK
        }
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("set_node_drained failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
