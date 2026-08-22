//! Repository routes: register + list.

use std::sync::Arc;

use agentgrid_common::{CreateRepositoryRequest, ListResponse, RepositoryView};
use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    Json,
};

use crate::auth::AuthedUser;
use crate::routes::WorkflowRunsQuery;
use crate::AppState;

pub async fn create_repository(
    State(state): State<Arc<AppState>>,
    auth: Option<Extension<AuthedUser>>,
    Json(req): Json<CreateRepositoryRequest>,
) -> Result<(StatusCode, Json<RepositoryView>), (StatusCode, Json<serde_json::Value>)> {
    // Hardening P1 item 32: vet the git_url scheme at the trust boundary so an
    // operator cannot register a `javascript:`/`data:`/arbitrary URI git remote.
    // Allow only git transports; `file://` is permitted for trusted local clones
    // (test repos) — narrowing `file://` to a policy flag is a P1 follow-up.
    if let Err(msg) = validate_git_url(&req.git_url) {
        tracing::warn!("create_repository rejected git_url: {msg}");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": msg})),
        ));
    }
    match state.store.create_repository(&req).await {
        Ok(v) => {
            let _ = state
                .store
                .audit(
                    "user",
                    auth.as_ref().map(|e| e.0.username.as_str()),
                    "repo.add",
                    Some(&v.id),
                    None,
                )
                .await;
            Ok((StatusCode::CREATED, Json(v)))
        }
        Err(e) => {
            tracing::error!("create_repository failed: {e}");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal error"})),
            ))
        }
    }
}

/// Hardening P1 item 32: accept only git-class URL schemes. Returns Err(msg)
/// for a scheme that could never be a legitimate git remote or that carries
/// injection risk (`javascript:`, `data:`, ...). `scp`-style (`user@host:path`)
/// and bare relative paths are allowed as git does.
fn validate_git_url(url: &str) -> Result<(), &'static str> {
    if url.is_empty() || url.trim().is_empty() {
        return Err("git_url must not be empty");
    }
    if url.contains('\n') || url.contains('\r') {
        return Err("git_url must not contain newlines");
    }
    if let Some(idx) = url.find(':') {
        if url[idx..].starts_with("://") {
            if !matches!(&url[..idx], "http" | "https" | "git" | "ssh" | "file") {
                return Err("git_url scheme not allowed");
            }
        } else if !url[..idx].contains('@') {
            // single colon, no `@host` -> a `scheme:path` URI (javascript:/data:),
            // which git never accepts. scp-style `user@host:path` is fine.
            return Err("git_url scheme not allowed");
        }
    }
    Ok(())
}

pub async fn list_repositories(
    State(state): State<Arc<AppState>>,
    Query(q): Query<WorkflowRunsQuery>,
) -> Result<Json<ListResponse<RepositoryView>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — surface
    // storage outage as 503 so the client does not read it as "no repos".
    let after = match (&q.after_created_at, &q.after_id) {
        (Some(c), Some(i)) if !c.is_empty() && !i.is_empty() => Some((c.clone(), i.clone())),
        _ => None,
    };
    match state.store.list_repositories(after, q.limit).await {
        Ok(items) => {
            let next_cursor = if items.len() == q.limit.unwrap_or(100).min(1000) as usize {
                items.last().map(|r| format!("{},{}", r.created_at, r.id))
            } else {
                None
            };
            Ok(Json(ListResponse::new(items, next_cursor)))
        }
        Err(e) => {
            tracing::error!("list_repositories failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}
