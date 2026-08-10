//! Plan 2.1 (#18): org-agent routes — create/list agents, view the action
//! trail, create tasks attributed to an agent (budget hard-stop).

use std::sync::Arc;

use agentgrid_common::{Agent, AgentAction, AgentCreate, CreateTaskRequest, TaskView};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};

use crate::AppState;

/// `POST /v1/agents` — register a long-lived org agent.
pub async fn create_agent(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AgentCreate>,
) -> Result<(StatusCode, Json<Agent>), StatusCode> {
    match state.store.create_agent(&req).await {
        Ok(agent) => Ok((StatusCode::CREATED, Json(agent))),
        Err(e) => {
            tracing::warn!("create_agent failed: {e}");
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

/// `GET /v1/agents` — list agents with current spend.
pub async fn list_agents(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Agent>>, StatusCode> {
    state
        .store
        .list_agents()
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// `GET /v1/agents/{id}/actions` — the agent's immutable trail.
pub async fn agent_actions(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<AgentAction>>, StatusCode> {
    state
        .store
        .agent_actions(&agent_id)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// `POST /v1/agents/{id}/tasks` — create a task attributed to the agent;
/// rejects (409) when the agent's budget is exhausted.
pub async fn create_agent_task(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<CreateTaskRequest>,
) -> Result<(StatusCode, Json<TaskView>), StatusCode> {
    match state.store.create_agent_task(&agent_id, &req).await {
        Ok(task) => Ok((StatusCode::CREATED, Json(task))),
        Err(e) => {
            tracing::warn!("create_agent_task failed: {e}");
            if e.to_string().contains("budget exhausted") || e.to_string().contains("unknown agent")
            {
                Err(StatusCode::CONFLICT)
            } else {
                Err(StatusCode::BAD_REQUEST)
            }
        }
    }
}
