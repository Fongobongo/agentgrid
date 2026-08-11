//! Conversation routes: stateful multi-turn chat routed to an agent.

use std::sync::Arc;

use agentgrid_common::{
    AppendMessageRequest, Conversation, ConversationMessage, CreateConversationRequest,
    CreateTaskRequest, ListResponse,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};

use crate::AppState;

pub async fn create_conversation(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateConversationRequest>,
) -> (StatusCode, Json<Conversation>) {
    match state
        .store
        .create_conversation(&req.adapter, &req.repository)
        .await
    {
        Ok(c) => (StatusCode::CREATED, Json(c)),
        Err(e) => {
            tracing::error!("create_conversation failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(Conversation {
                    id: String::new(),
                    adapter: String::new(),
                    repository: String::new(),
                    created_at: String::new(),
                }),
            )
        }
    }
}

pub async fn show_conversation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Conversation>, StatusCode> {
    state
        .store
        .get_conversation(&id)
        .await
        .map_err(|e| {
            tracing::error!("get_conversation failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Compose the conversation history into a prompt the agent receives, so any
/// node picking the task up sees the full shared context. Format is a simple
/// transcript: `user:` / `assistant:` lines.
fn compose_conversation_prompt(messages: &[ConversationMessage], new_user: &str) -> String {
    let mut s = String::new();
    for m in messages {
        s.push_str(m.role.as_str());
        s.push_str(": ");
        s.push_str(&m.content);
        s.push('\n');
    }
    s.push_str("user: ");
    s.push_str(new_user);
    s
}

/// Append a user message and create a task carrying the composed conversation
/// prompt. The task is assigned by the scheduler to any node serving
/// `adapter`+`repository`. Returns the task id so the gateway can stream the
/// answer.
pub async fn append_conversation_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AppendMessageRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    let conv = state
        .store
        .get_conversation(&id)
        .await
        .map_err(|e| {
            tracing::error!("get_conversation failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    let history = state
        .store
        .list_conversation_messages(&id, 0, 1000)
        .await
        .map_err(|e| {
            tracing::error!("list_conversation_messages failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let prompt = compose_conversation_prompt(&history, &req.content);
    // Stage 11.5: if a prior turn finished an ACP session, resume it so the
    // agent does not re-process the transcript from scratch.
    let parent_acp_session_id = state
        .store
        .last_conversation_acp_session(&id)
        .await
        .map_err(|e| {
            tracing::warn!("last_conversation_acp_session failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let task_req = CreateTaskRequest {
        prompt,
        repository: conv.repository.clone(),
        adapter: conv.adapter.clone(),
        requested_node_id: None,
        timeout_secs: None,
        validation_command: None,
        base_commit: None,
        parent_acp_session_id,
        security_profile: None,
        network_mode: None,
        group_id: None,
        agent_id: None,
        consensus_group_id: None,
        consensus_member: None,
    };
    let task = state.store.create_task(&task_req).await.map_err(|e| {
        tracing::error!("create_task for conversation failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    state
        .store
        .append_conversation_message(&id, "user", &req.content, Some(&task.id))
        .await
        .map_err(|e| {
            tracing::error!("append user message failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"task_id": task.id, "conversation_id": id})),
    ))
}

pub async fn list_conversation_messages(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<ListResponse<ConversationMessage>>, StatusCode> {
    let after_seq = params
        .get("after_seq")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let limit = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    match state
        .store
        .list_conversation_messages(&id, after_seq, limit)
        .await
    {
        Ok(items) => {
            let next_cursor = if items.len() == limit as usize {
                items.last().map(|m| m.seq.to_string())
            } else {
                None
            };
            Ok(Json(ListResponse::new(items, next_cursor)))
        }
        Err(e) => {
            tracing::error!("list_conversation_messages failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
