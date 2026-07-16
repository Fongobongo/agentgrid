//! Shared types for agentgrid: task/attempt/node status enums, the adapter
//! event model, and the API DTOs exchanged between control plane, node daemon
//! and CLI.

use serde::{Deserialize, Serialize};

/// Task lifecycle status (control-plane view of a user request).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Assigned,
    Running,
    Validating,
    Succeeded,
    Failed,
    Cancelled,
}

/// Per-attempt status (one execution of a task on a node).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Assigned,
    Running,
    Validating,
    Succeeded,
    Failed,
    Cancelled,
    Lost,
}

/// Node registration/health status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Pending,
    Online,
    Degraded,
    Offline,
    Revoked,
}

/// Stored event kinds. Mirrors the spec's `status | stdout | stderr | tool |
/// artifact | metric` plus `result`/`error` carried over from the adapter
/// contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Status,
    Stdout,
    Stderr,
    Tool,
    Artifact,
    Metric,
    Result,
    Error,
}

/// A single streamed event tied to an attempt, with a monotonic `sequence`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskEvent {
    pub attempt_id: String,
    pub sequence: u64,
    pub r#type: EventType,
    pub payload: serde_json::Value,
    pub created_at: String,
}

macro_rules! display_snake {
    ($t:ty) => {
        impl std::fmt::Display for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let s = serde_json::to_value(self)
                    .map_err(|_| std::fmt::Error)?
                    .as_str()
                    .ok_or(std::fmt::Error)?
                    .to_string();
                f.write_str(&s)
            }
        }
    };
}
display_snake!(TaskStatus);
display_snake!(AttemptStatus);
display_snake!(NodeStatus);
display_snake!(EventType);

// ----- API DTOs -----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateTaskRequest {
    pub prompt: String,
    pub repository: String,
    pub adapter: String,
    #[serde(default)]
    pub requested_node_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskView {
    pub id: String,
    pub repository: String,
    pub prompt: String,
    pub adapter: String,
    pub status: TaskStatus,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub assigned_attempt_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeView {
    pub id: String,
    pub name: String,
    pub status: NodeStatus,
    pub adapters: Vec<String>,
    pub repositories: Vec<String>,
    pub max_concurrency: u32,
    pub active_attempts: u32,
    pub last_heartbeat_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollRequest {
    pub node_id: String,
    pub name: String,
    pub adapters: Vec<String>,
    pub repositories: Vec<String>,
    pub max_concurrency: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    pub attempt_id: String,
    pub task_id: String,
    pub repository: String,
    pub prompt: String,
    pub adapter: String,
    pub number: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollResponse {
    pub assignment: Option<Assignment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IncomingEvent {
    pub sequence: u64,
    pub r#type: EventType,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestEventsRequest {
    pub events: Vec<IncomingEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompleteAttemptRequest {
    pub exit_code: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventsQuery {
    #[serde(default)]
    pub after_sequence: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: Serialize + for<'de> Deserialize<'de>>(v: &T) -> T {
        let s = serde_json::to_string(v).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn status_enums_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&TaskStatus::Succeeded).unwrap(),
            "\"succeeded\""
        );
        let s: TaskStatus = serde_json::from_str("\"failed\"").unwrap();
        assert_eq!(s, TaskStatus::Failed);
        let a: AttemptStatus = serde_json::from_str("\"lost\"").unwrap();
        assert_eq!(a, AttemptStatus::Lost);
        let n: NodeStatus = serde_json::from_str("\"degraded\"").unwrap();
        assert_eq!(n, NodeStatus::Degraded);
    }

    #[test]
    fn event_type_round_trip() {
        let e = EventType::Stdout;
        assert_eq!(round_trip(&e), e);
        let e = EventType::Result;
        assert_eq!(round_trip(&e), e);
    }

    #[test]
    fn dto_round_trip() {
        let req = CreateTaskRequest {
            prompt: "write:hello.txt:hi".into(),
            repository: "demo".into(),
            adapter: "mock".into(),
            requested_node_id: Some("node-1".into()),
        };
        assert_eq!(round_trip(&req), req);

        let ev = TaskEvent {
            attempt_id: "a1".into(),
            sequence: 3,
            r#type: EventType::Stdout,
            payload: serde_json::json!({"text": "hi"}),
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        assert_eq!(round_trip(&ev), ev);

        let pr = PollResponse {
            assignment: Some(Assignment {
                attempt_id: "a1".into(),
                task_id: "t1".into(),
                repository: "demo".into(),
                prompt: "x".into(),
                adapter: "mock".into(),
                number: 1,
            }),
        };
        assert_eq!(round_trip(&pr), pr);
    }
}
