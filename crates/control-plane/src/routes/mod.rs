//! HTTP route handlers, grouped by resource. Handlers stay thin:
//! auth (middleware/extensions) → validate → store call → response.

pub mod approvals;
pub mod artifacts;
pub mod nodes;
pub mod tasks;
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
