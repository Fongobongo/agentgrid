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

mod state_machine;

pub use state_machine::{
    next_attempt_status, next_task_status, AttemptTransition, InvalidTransition, TaskTransition,
};

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
    /// Optional per-task timeout in seconds (server default if unset).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
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
    pub agent_version: String,
    pub load_avg: f64,
    pub free_disk_mb: u64,
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
    /// Seconds before the node should forcibly kill the attempt.
    pub timeout_secs: u64,
    /// Git remote URL; empty when the task runs in a plain directory.
    #[serde(default)]
    pub git_url: String,
    /// Branch new attempts branch from (e.g. `main`).
    #[serde(default)]
    pub default_branch: String,
    /// Optional validation command run after the agent succeeds (Stage 3.3).
    #[serde(default)]
    pub validation_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelState {
    pub cancel_requested: bool,
}

/// One-time enrollment token issued by an admin (TTL 10 min, only hash stored).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrollTokenResponse {
    pub token: String,
    pub expires_at: String,
}

/// Stage 4.1: create the first local user (only allowed while no users exist).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetupRequest {
    pub username: String,
    pub password: String,
}

/// Stage 4.1: username + password exchange for a JWT.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// Stage 4.1: JWT returned on successful login.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: String,
}

/// Exchange an enrollment token for a permanent node credential.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub token: String,
    pub name: String,
    #[serde(default)]
    pub adapters: Vec<String>,
    #[serde(default)]
    pub repositories: Vec<String>,
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: u32,
    #[serde(default)]
    pub agent_version: String,
}

/// Node identity + secret credential returned once at enroll (never stored plaintext).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub node_id: String,
    pub credential: String,
}

/// Periodic node health/capability report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    #[serde(default)]
    pub status: Option<NodeStatus>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub adapters: Vec<String>,
    #[serde(default)]
    pub repositories: Vec<String>,
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: u32,
    #[serde(default)]
    pub agent_version: String,
    #[serde(default)]
    pub load_avg: f64,
    #[serde(default)]
    pub free_disk_mb: u64,
    #[serde(default)]
    pub active_attempts: u32,
}

fn default_max_concurrency() -> u32 {
    1
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
    /// Commit SHA produced by the attempt, if it ran in a git worktree.
    #[serde(default)]
    pub commit_sha: Option<String>,
    /// Distinct failure category: `agent_failed` vs `validation_failed` etc.
    #[serde(default)]
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateRepositoryRequest {
    pub name: String,
    pub git_url: String,
    pub default_branch: String,
    #[serde(default)]
    pub validation_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepositoryView {
    pub id: String,
    pub name: String,
    pub git_url: String,
    pub default_branch: String,
    pub validation_command: Option<String>,
    pub created_at: String,
}

/// Per-node eligibility for a (repository, adapter) pair, with reasons when not
/// eligible (Stage 2.4 `no_eligible_nodes` visibility).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeEligibility {
    pub node_id: String,
    pub status: NodeStatus,
    pub eligible: bool,
    pub reasons: Vec<String>,
}

/// Why a queued task has no eligible node, plus per-node detail (Stage 2.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskEligibility {
    pub task_id: String,
    /// Distinct reasons no node can run the task; empty when at least one node is eligible.
    pub no_eligible_nodes: Vec<String>,
    pub nodes: Vec<NodeEligibility>,
}

/// Upload a text artifact (e.g. `changes.patch`) from a node to the control plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadArtifactRequest {
    pub name: String,
    pub content: String,
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
            timeout_secs: None,
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
                timeout_secs: 3600,
                git_url: String::new(),
                default_branch: String::new(),
                validation_command: None,
            }),
        };
        assert_eq!(round_trip(&pr), pr);
    }

    #[test]
    fn enroll_dto_round_trip() {
        let er = EnrollRequest {
            token: "t".into(),
            name: "n".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            agent_version: "0.1".into(),
        };
        assert_eq!(round_trip(&er), er);
        let hb = HeartbeatRequest {
            status: Some(NodeStatus::Online),
            name: "n".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            agent_version: "0.1".into(),
            load_avg: 0.5,
            free_disk_mb: 1024,
            active_attempts: 1,
        };
        assert_eq!(round_trip(&hb), hb);
        let resp = EnrollResponse {
            node_id: "node-1".into(),
            credential: "secret".into(),
        };
        assert_eq!(round_trip(&resp), resp);
    }
}
