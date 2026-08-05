//! Policy, skill-trust, MCP-server, and agent-profile routes.

use std::sync::Arc;

use agentgrid_common::{McpServer, McpServerCreate};
use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    Json,
};

use crate::auth::AuthedUser;
use crate::AppState;

pub async fn evaluate_policy(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EvaluatePolicyRequest>,
) -> Json<agentgrid_common::PolicyVerdict> {
    let level = req.autonomy.unwrap_or_default();
    let verdict = agentgrid_common::BuiltinPolicyProvider::new()
        .evaluate_with(level, &req.command, &req.cwd)
        .unwrap_or_else(|e| agentgrid_common::PolicyVerdict::fail_closed(&e.0));
    // Fail-closed audit: every policy decision is recorded so dangerous commands
    // are never silent.
    let payload = serde_json::to_string(&verdict).unwrap_or_else(|_| "{}".to_string());
    let _ = state
        .store
        .audit(
            "system",
            None,
            "policy.evaluate",
            Some(&req.command),
            Some(&payload),
        )
        .await;
    Json(verdict)
}

#[derive(serde::Deserialize)]
pub(crate) struct EvaluatePolicyRequest {
    command: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    autonomy: Option<agentgrid_common::AutonomyLevel>,
}

// ---- Skill trust (Stage 9.2) ----

/// Query param for listing trust: `?source=project` filters by source tier.
#[derive(serde::Deserialize)]
pub(crate) struct SkillTrustQuery {
    source: Option<String>,
}

pub async fn list_skills_trust_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SkillTrustQuery>,
) -> Result<Json<Vec<agentgrid_common::SkillTrustView>>, StatusCode> {
    let rows = state.store.list_skill_trust().await.map_err(|e| {
        tracing::error!("list_skill_trust failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(match q.source {
        Some(s) => rows.into_iter().filter(|r| r.source == s).collect(),
        None => rows,
    }))
}

pub async fn get_skill_trust_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SkillTrustQuery>,
    Path(name): Path<String>,
) -> Result<Json<agentgrid_common::SkillTrustView>, StatusCode> {
    let source = q.source.unwrap_or_else(|| "project".to_string());
    state
        .store
        .get_skill_trust(&name, &source)
        .await
        .map(Json)
        .map_err(|e| {
            tracing::error!("get_skill_trust failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

pub async fn trust_skill_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Query(q): Query<SkillTrustQuery>,
    Path(name): Path<String>,
) -> StatusCode {
    set_skill_trust(state, auth, &name, q.source.as_deref(), true).await
}

pub async fn untrust_skill_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Query(q): Query<SkillTrustQuery>,
    Path(name): Path<String>,
) -> StatusCode {
    set_skill_trust(state, auth, &name, q.source.as_deref(), false).await
}

async fn set_skill_trust(
    state: Arc<AppState>,
    auth: Option<Extension<AuthedUser>>,
    name: &str,
    source: Option<&str>,
    trusted: bool,
) -> StatusCode {
    let actor = auth
        .as_ref()
        .map(|e| e.0.username.as_str())
        .unwrap_or("system");
    let source = source.unwrap_or("project");
    if let Err(e) = state
        .store
        .set_skill_trust(name, source, trusted, actor)
        .await
    {
        tracing::error!("set_skill_trust failed: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    let _ = state
        .store
        .audit(
            actor,
            None,
            "skill.trust",
            Some(&format!("{name}/{source}")),
            Some(if trusted { "trusted" } else { "untrusted" }),
        )
        .await;
    StatusCode::OK
}

/// Stage 13: list all registered MCP servers.
pub async fn list_mcp_servers_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<McpServer>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — surface
    // storage outage as 503 so the client does not read it as "no servers".
    match state.store.list_mcp_servers().await {
        Ok(s) => Ok(Json(s)),
        Err(e) => {
            tracing::error!("list_mcp_servers failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// Stage 13: register (or replace) an MCP server in the operator registry.
pub async fn create_mcp_server_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<McpServerCreate>,
) -> Result<Json<McpServer>, StatusCode> {
    state
        .store
        .upsert_mcp_server(&req)
        .await
        .map(Json)
        .map_err(|e| {
            tracing::warn!("create_mcp_server failed: {e}");
            StatusCode::BAD_REQUEST
        })
}

/// Stage 13: delete an MCP server from the registry.
pub async fn delete_mcp_server_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> StatusCode {
    match state.store.delete_mcp_server(&id).await {
        Ok(true) => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            tracing::error!("delete_mcp_server failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

// ---- Agent profiles (Stage 13) ----

pub async fn list_profiles_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<String>>, StatusCode> {
    state.store.list_profiles().await.map(Json).map_err(|e| {
        tracing::error!("list_profiles failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

pub async fn get_profile_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<agentgrid_common::AgentProfile>>, StatusCode> {
    state
        .store
        .list_profile_revisions(&id)
        .await
        .map(Json)
        .map_err(|e| {
            tracing::error!("get_profile failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

pub async fn create_profile_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Path(id): Path<String>,
    Json(body): Json<agentgrid_common::AgentProfileCreate>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let actor = auth
        .as_ref()
        .map(|e| e.0.username.as_str())
        .unwrap_or("system");
    match state.store.create_profile_revision(&id, &body, actor).await {
        Ok(rev) => {
            let _ = state
                .store
                .audit(
                    actor,
                    None,
                    "profile.create",
                    Some(&format!("{id}/{rev}")),
                    None,
                )
                .await;
            Ok(Json(serde_json::json!({ "id": id, "revision": rev })))
        }
        Err(e) => {
            tracing::error!("create_profile failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

pub async fn activate_profile_handler(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Path(id): Path<String>,
    Json(body): Json<agentgrid_common::ActivateProfile>,
) -> StatusCode {
    let actor = auth
        .as_ref()
        .map(|e| e.0.username.as_str())
        .unwrap_or("system");
    if let Err(e) = state.store.activate_profile(&id, body.revision).await {
        tracing::error!("activate_profile failed: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    let _ = state
        .store
        .audit(
            actor,
            None,
            "profile.activate",
            Some(&format!("{id}/{}", body.revision)),
            None,
        )
        .await;
    StatusCode::OK
}
