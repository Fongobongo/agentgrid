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
    response::{IntoResponse, Response},
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

/// `PUT /v1/opencode-profiles/{name}?dry_run=true` — exercise normalize_config
/// against the posted body *without writing*. Returns the effective config
/// (post-sanitisation), the hash that WOULD have been computed, and the
/// list of top-level keys stripped as unknown — so the web editor can show
/// the operator exactly what their JSON becomes before they hit save.
#[derive(Deserialize)]
pub struct UpsertQuery {
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(serde::Serialize)]
pub struct UpsertDryRun {
    pub would_set_hash: String,
    pub effective_config: serde_json::Value,
    pub dropped_keys: Vec<String>,
}

fn dry_run_response(config: serde_json::Value) -> Result<Json<UpsertDryRun>, StatusCode> {
    use crate::store::opencode_profiles::{last_dropped_keys, sanitize_config};

    let effective = sanitize_config(&config).map_err(|_| StatusCode::BAD_REQUEST)?;
    let dropped = last_dropped_keys();
    let json_string = serde_json::to_string(&effective).map_err(|_| StatusCode::BAD_REQUEST)?;
    use sha2::Digest;
    let hash = format!("{:x}", sha2::Sha256::digest(json_string.as_bytes()));
    Ok(Json(UpsertDryRun {
        would_set_hash: hash,
        effective_config: effective,
        dropped_keys: dropped,
    }))
}

/// `PUT /v1/opencode-profiles/{name}` — create-or-replace. On successful
/// write the CP pushes ConfigUpdate to every node currently subscribed.
///
/// `If-Match` (or `x-expected-hash` for clients that don't speak RFC 9110):
/// when present, the PUT refuses to commit unless the profile's current
/// hash equals the header's value. Two operators racing on the same profile
/// get a 409 + the current hash back, the loser has to re-fetch.
pub async fn upsert_profile(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<UpsertQuery>,
    Extension(auth): Extension<crate::auth::AuthedUser>,
    headers: axum::http::HeaderMap,
    Json(body): Json<UpsertOpencodeProfileRequest>,
) -> Result<Response, StatusCode> {
    if q.dry_run {
        return dry_run_response(body.config).map(|j| j.into_response());
    }
    // Optimistic-concurrency: when the client sent If-Match we check the
    // profile's current hash before writing.
    let expected = headers
        .get(axum::http::header::IF_MATCH)
        .or_else(|| headers.get("x-expected-hash"))
        .and_then(|h| h.to_str().ok())
        .map(|s| s.trim().trim_matches('"').to_string());
    if let Some(expected) = expected {
        let current = state
            .store
            .get_opencode_profile(&name)
            .await
            .ok()
            .flatten()
            .map(|p| p.hash);
        match current {
            Some(current) if current == expected => {}
            Some(current) => {
                let mut resp = Json(serde_json::json!({
                    "error": "the profile changed since your last read",
                    "current_hash": current
                }))
                .into_response();
                *resp.status_mut() = StatusCode::CONFLICT;
                return Ok(resp);
            }
            None => {
                // PUT-against-nonexistent: pass (the upsert creates).
            }
        }
    }
    // Expiry is validated loudly (RFC3339) so a typo'd TTL surfaces here
    // instead of silently never expiring.
    if let Some(ea) = &body.expires_at {
        chrono::DateTime::parse_from_rfc3339(ea).map_err(|e| {
            tracing::warn!("opencode upsert bad expires_at: {e}");
            StatusCode::BAD_REQUEST
        })?;
    }
    let profile = state
        .store
        .upsert_opencode_profile(&name, body.config, body.expires_at)
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
    // Operator attribution: each PUT lands an audit row keyed by user.
    let detail = serde_json::json!({
        "profile_id": profile.id,
        "hash": profile.hash,
        "prev_hash": profile.prev.as_ref().map(|p| p.hash.clone()),
    })
    .to_string();
    let _ = state
        .store
        .audit(
            "user",
            Some(&auth.username),
            "opencode.upsert",
            None,
            Some(&detail),
        )
        .await;
    Ok(Json(profile).into_response())
}

/// `DELETE /v1/opencode-profiles/{name}` — remove the profile. Nodes keep
/// their last-applied on-disk config (harmless); the FK pointer is cleared
/// via ON DELETE SET NULL. With `?fallback=<other>` the delete first
/// re-points every assigned node onto that profile atomically (and the push
/// carries the fallback's hash so nodes apply it immediately).
#[derive(Deserialize)]
pub struct DeleteQuery {
    pub fallback: Option<String>,
}

pub async fn delete_profile(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<crate::auth::AuthedUser>,
    Path(name): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Result<StatusCode, StatusCode> {
    // Snapshot the affected nodes BEFORE the delete so the push can wake
    // them into dropping the profile voluntarily.
    let profile_id: Option<String> = match state.store.get_opencode_profile(&name).await {
        Ok(Some(p)) => Some(p.id),
        _ => None,
    };
    let affected: Vec<String> = match &profile_id {
        Some(pid) => state
            .store
            .list_nodes_for_profile(pid)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };

    // Optional explicit fallback: reassign affected nodes to another
    // profile before deleting — no auto-magic, the operator names it.
    let (fallback_id, fallback_hash): (Option<String>, Option<String>) =
        if let Some(fb) = q.fallback.as_deref() {
            if fb == name {
                return Err(StatusCode::BAD_REQUEST);
            }
            match state.store.get_opencode_profile(fb).await {
                Ok(Some(p)) => (Some(p.id), Some(p.hash)),
                _ => return Err(StatusCode::NOT_FOUND),
            }
        } else {
            (None, None)
        };

    let deleted = match &fallback_id {
        Some(fid) => {
            state
                .store
                .delete_opencode_profile_with_fallback(&name, fid)
                .await
        }
        None => state.store.delete_opencode_profile(&name).await,
    };
    match deleted {
        Ok(true) => {
            // With fallback the push carries the new hash so nodes apply
            // the replacement instead of merely dropping the old one.
            push_config_update(&state, &affected, fallback_id, fallback_hash).await;
            let detail = serde_json::json!({
                "profile_name": name,
                "affected_nodes": affected.len(),
                "fallback": q.fallback,
            })
            .to_string();
            let _ = state
                .store
                .audit(
                    "user",
                    Some(&auth.username),
                    "opencode.delete",
                    None,
                    Some(&detail),
                )
                .await;
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

/// `POST /v1/opencode-profiles/{name}/rollback` — swap the profile back N
/// revisions. `?steps=n` walks the revision stack (default 1, capped at 32
/// so an accidental endless-loop input can't spin the store).
#[derive(Deserialize)]
pub struct RollbackQuery {
    #[serde(default = "default_steps")]
    pub steps: u32,
}
fn default_steps() -> u32 {
    1
}

pub async fn rollback_profile(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<crate::auth::AuthedUser>,
    Path(name): Path<String>,
    Query(q): Query<RollbackQuery>,
) -> Result<Json<OpencodeProfile>, StatusCode> {
    let profile = state
        .store
        .rollback_opencode_profile(&name, q.steps.min(32))
        .await
        .map_err(|e| {
            tracing::error!("rollback_opencode_profile: {e}");
            StatusCode::SERVICE_UNAVAILABLE
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    // Operator attribution for the rollback — the existing generic audit
    // feed records who swivelled the profile so a multi-operator shop can
    // diff "I was testing" from "colleague reverted me".
    let detail = serde_json::json!({
        "profile_id": profile.id,
        "hash": profile.hash,
        "steps": q.steps,
    })
    .to_string();
    let _ = state
        .store
        .audit(
            "user",
            Some(&auth.username),
            "opencode.rollback",
            None,
            Some(&detail),
        )
        .await;
    // Push the swap out to every node assigned to this profile.
    let affected = state
        .store
        .list_nodes_for_profile(&profile.id)
        .await
        .unwrap_or_default();
    if !affected.is_empty() {
        push_config_update(
            &state,
            &affected,
            Some(profile.id.clone()),
            Some(profile.hash.clone()),
        )
        .await;
    }
    Ok(Json(profile))
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

/// `POST /v1/node/opencode-config/audit` — node records that an apply just
/// happened. The body carries the hash + trigger vocabulary used by the node;
/// the CP inserts it (after basic trigger validation) keyed by AuthedNode.
#[derive(Deserialize)]
pub struct AuditPostBody {
    /// sha256 of the config currently active on the node.
    pub hash: String,
    /// Trigger vocabulary: ws_push | error_threshold | interval | startup.
    pub trigger: String,
    /// Profile id the apply was attributed to (None when the node has no profile).
    #[serde(default)]
    pub profile_id: Option<String>,
    /// Outcome of the node-side `opencode debug config` oracle:
    /// verified | skipped_no_binary | verify_failed | unknown.
    #[serde(default)]
    pub verify: Option<String>,
}

pub async fn record_audit(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Json(body): Json<AuditPostBody>,
) -> Result<StatusCode, StatusCode> {
    const VALID: &[&str] = &["ws_push", "error_threshold", "interval", "startup"];
    const VALID_VERIFY: &[&str] = &["verified", "skipped_no_binary", "verify_failed", "unknown"];
    if !VALID.contains(&body.trigger.as_str()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if body.hash.len() > 128 {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(v) = &body.verify {
        if !VALID_VERIFY.contains(&v.as_str()) {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    state
        .store
        .record_opencode_apply(
            &auth.node_id,
            body.profile_id.as_deref(),
            &body.hash,
            &body.trigger,
            body.verify.as_deref(),
        )
        .await
        .map_err(|e| {
            tracing::error!("record_opencode_apply: {e}");
            StatusCode::SERVICE_UNAVAILABLE
        })?;
    Ok(StatusCode::NO_CONTENT)
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

/// `POST /v1/opencode-profiles/{name}/assign-percent` — A/B split. Moves
/// the nodes currently on either arm (`{name}` and the body's `other`) so
/// that `percent`% land on `{name}` and the rest on `other`. Deterministic
/// (ordered by node id) so re-running with the same percent is stable;
/// only nodes already on one of the two arms move, the rest of the fleet
/// is left alone.
#[derive(Deserialize)]
pub struct AssignPercentBody {
    pub other: String,
    /// Share of nodes for the path-side profile (0-100).
    pub percent: u8,
}

pub async fn assign_percent(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<crate::auth::AuthedUser>,
    Path(name): Path<String>,
    Json(body): Json<AssignPercentBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if body.other == name {
        return Err(StatusCode::BAD_REQUEST);
    }
    if body.percent > 100 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let keep = state
        .store
        .get_opencode_profile(&name)
        .await
        .ok()
        .flatten()
        .ok_or(StatusCode::NOT_FOUND)?;
    let other = state
        .store
        .get_opencode_profile(&body.other)
        .await
        .ok()
        .flatten()
        .ok_or(StatusCode::NOT_FOUND)?;
    let moves = state
        .store
        .assign_percent_between(&keep.id, &other.id, body.percent)
        .await
        .map_err(|e| {
            tracing::error!("assign_percent_between: {e}");
            StatusCode::SERVICE_UNAVAILABLE
        })?;
    // Push the matching hash to every moved node so it applies its arm.
    for (node_id, profile_id) in &moves {
        let (pid, hash) = if profile_id == &keep.id {
            (Some(keep.id.clone()), Some(keep.hash.clone()))
        } else {
            (Some(other.id.clone()), Some(other.hash.clone()))
        };
        state
            .ws_registry
            .send(
                node_id,
                &NodeWsMsg::ConfigUpdate {
                    profile_id: pid,
                    hash,
                },
            )
            .await;
    }
    let detail = serde_json::json!({
        "profile_name": name,
        "other": body.other,
        "percent": body.percent,
        "moved": moves.len(),
    })
    .to_string();
    let _ = state
        .store
        .audit(
            "user",
            Some(&auth.username),
            "opencode.assign_percent",
            None,
            Some(&detail),
        )
        .await;
    Ok(Json(serde_json::json!({
        "moved": moves.len(),
        "keep_id": keep.id,
        "other_id": other.id,
        "keep_percent": body.percent,
    })))
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
