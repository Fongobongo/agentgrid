//! HTTP route handlers, grouped by resource. Handlers stay thin:
//! auth (middleware/extensions) → validate → store call → response.

pub mod agents;
pub mod approvals;
pub mod artifacts;
pub mod attempts;
pub mod conversations;
pub mod events;
pub mod learnings;
pub mod maintenance;
pub mod nodes;
pub mod opencode;
pub mod profiles;
pub mod repositories;
pub mod shared_context;
pub mod tasks;
pub mod users;
pub mod webhooks;
pub mod workflows;

/// Hardening P2 item 20: keyset pagination query (`after_created_at` +
/// `after_id` cursor + server page cap). Shared by nodes/workflows/repos.
#[derive(Debug, Default, serde::Deserialize)]
pub struct WorkflowRunsQuery {
    #[serde(default)]
    pub after_created_at: Option<String>,
    #[serde(default)]
    pub after_id: Option<String>,
    #[serde(default)]
    pub limit: Option<u64>,
}
