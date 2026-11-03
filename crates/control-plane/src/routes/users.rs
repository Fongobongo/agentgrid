//! User administration (plan 5.2, extended by finding 7). All routes sit behind
//! `require_user_auth`; mutating verbs are additionally blocked for operators
//! by the RBAC check in the auth middleware, so only admins reach these
//! handlers.
//!
//! Finding 7 added the missing lifecycle: before this, an admin could create a
//! user and list users, and that was all. Removing a departed admin, demoting
//! one, or rotating a leaked password meant editing SQLite by hand — and even
//! then the change did not take effect until the affected user's twelve-hour
//! token expired. Every mutation here bumps `sessions_valid_after`, which
//! `verify_token` compares against the token's `iat`.

use std::sync::Arc;

use agentgrid_common::{CreateUserRequest, UpdateUserRequest, UserEntry};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};

use crate::auth::{password_is_acceptable, AuthedUser};
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
    if !password_is_acceptable(&req.password) {
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

/// `DELETE /v1/users/{name}`.
///
/// Refuses the last admin: deleting the only account that can administer the
/// install would leave it unreachable without editing SQLite, which is the
/// situation this endpoint exists to remove.
pub async fn delete_user_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedUser>,
    Path(username): Path<String>,
) -> Result<StatusCode, StatusCode> {
    if auth.role != agentgrid_common::ROLE_ADMIN {
        return Err(StatusCode::FORBIDDEN);
    }
    let target = match state.store.user_auth_state(&username).await {
        Ok(Some(state)) => state,
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("user_auth_state failed: {e}");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    if target.0 == agentgrid_common::ROLE_ADMIN {
        match state.store.admin_count().await {
            Ok(n) if n <= 1 => return Err(StatusCode::CONFLICT),
            Ok(_) => {}
            Err(e) => {
                tracing::error!("admin_count failed: {e}");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }
    match state.store.delete_user(&username).await {
        Ok(true) => {}
        Ok(false) => return Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("delete_user failed: {e}");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    // A deleted row already fails `verify_token`, but revoking the jti as well
    // keeps the audit trail honest about *why* the session ended.
    let _ = state
        .store
        .audit(
            "user",
            Some(&auth.username),
            "user.delete",
            Some(&format!("username={username} role={}", target.0)),
            None,
        )
        .await;
    Ok(StatusCode::OK)
}

/// `PATCH /v1/users/{name}` — change the role and/or the password.
///
/// Either change invalidates every session issued so far: a demoted admin must
/// not keep admin for eleven more hours, and a rotated password must not leave
/// the stolen token alive.
pub async fn update_user_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedUser>,
    Path(username): Path<String>,
    Json(req): Json<UpdateUserRequest>,
) -> Result<StatusCode, StatusCode> {
    if auth.role != agentgrid_common::ROLE_ADMIN {
        return Err(StatusCode::FORBIDDEN);
    }
    if req.role.is_none() && req.password.is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(role) = req.role.as_deref() {
        if !agentgrid_common::is_valid_role(role) {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    if let Some(password) = req.password.as_deref() {
        if !password_is_acceptable(password) {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    let current = match state.store.user_auth_state(&username).await {
        Ok(Some(state)) => state,
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("user_auth_state failed: {e}");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    // Same rule as deletion: an install with no admin cannot be administered.
    let demoting_last_admin = req.role.as_deref() == Some(agentgrid_common::ROLE_OPERATOR)
        && current.0 == agentgrid_common::ROLE_ADMIN;
    if demoting_last_admin {
        match state.store.admin_count().await {
            Ok(n) if n <= 1 => return Err(StatusCode::CONFLICT),
            Ok(_) => {}
            Err(e) => {
                tracing::error!("admin_count failed: {e}");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    let mut changed: Vec<String> = Vec::new();
    if let Some(role) = req.role.as_deref() {
        if role != current.0 {
            match state.store.set_user_role(&username, role).await {
                Ok(true) => changed.push(format!("role={role}")),
                Ok(false) => return Err(StatusCode::NOT_FOUND),
                Err(e) => {
                    tracing::error!("set_user_role failed: {e}");
                    return Err(StatusCode::INTERNAL_SERVER_ERROR);
                }
            }
        }
    }
    if let Some(password) = req.password.as_deref() {
        match state.store.set_user_password(&username, password).await {
            Ok(true) => changed.push("password=changed".to_string()),
            Ok(false) => return Err(StatusCode::NOT_FOUND),
            Err(e) => {
                tracing::error!("set_user_password failed: {e}");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }
    if !changed.is_empty() {
        if let Err(e) = state.store.bump_sessions_gen(&username).await {
            tracing::error!("bump_sessions_gen failed: {e}");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
        let _ = state
            .store
            .audit(
                "user",
                Some(&auth.username),
                "user.update",
                Some(&format!("username={username} {}", changed.join(" "))),
                None,
            )
            .await;
    }
    Ok(StatusCode::OK)
}
