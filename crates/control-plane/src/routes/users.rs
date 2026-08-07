//! User administration (plan 5.2). All routes sit behind `require_user_auth`;
//! mutating verbs are additionally blocked for operators by the RBAC check in
//! the auth middleware, so only admins reach these handlers.

use std::sync::Arc;

use agentgrid_common::{CreateUserRequest, UserEntry};
use axum::{extract::State, http::StatusCode, Extension, Json};

use crate::auth::AuthedUser;
use crate::AppState;

pub async fn list_users_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<UserEntry>>, StatusCode> {
    let users = state.store.list_users().await.map_err(|e| {
        tracing::error!("list_users failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(
        users
            .into_iter()
            .map(|(username, role)| UserEntry { username, role })
            .collect(),
    ))
}

pub async fn create_user_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedUser>,
    Json(req): Json<CreateUserRequest>,
) -> Result<StatusCode, StatusCode> {
    // Defense in depth: the RBAC middleware already blocks operator mutations.
    if auth.role != agentgrid_common::ROLE_ADMIN {
        return Err(StatusCode::FORBIDDEN);
    }
    if req.username.is_empty() || req.password.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if !agentgrid_common::is_valid_role(&req.role) {
        return Err(StatusCode::BAD_REQUEST);
    }
    match state
        .store
        .create_user(&req.username, &req.password, &req.role)
        .await
    {
        Ok(true) => {}
        Ok(false) => return Err(StatusCode::CONFLICT),
        Err(e) => {
            tracing::error!("create_user failed: {e}");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    let _ = state
        .store
        .audit(
            "user",
            Some(&auth.username),
            "user.create",
            Some(&format!("username={} role={}", req.username, req.role)),
            None,
        )
        .await;
    Ok(StatusCode::CREATED)
}
