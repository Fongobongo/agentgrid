//! Durable approval state machine + view (Stage 5 prerequisite, built before
//! any ACP integration). The control plane persists approvals in a table and
//! routes `session/request_permission` through this transition core.
//!
//! Fail-closed by policy: a `Pending` approval that is not explicitly `Allow`ed
//! ends `Denied`/`Expired` — there is no unconditional-allow shortcut.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalStatus {
    Pending,
    Allowed,
    Denied,
    Expired,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalEvent {
    Allow,
    Deny,
    Expire,
    Cancel,
}

#[derive(Debug, Error, PartialEq)]
#[error("invalid approval transition: {0:?} --{1:?}-->")]
pub struct InvalidApprovalTransition(pub ApprovalStatus, pub ApprovalEvent);

/// Apply an event to an approval. Only `Pending` is live; every terminal state
/// rejects further transitions.
pub fn next_approval(
    s: ApprovalStatus,
    e: ApprovalEvent,
) -> Result<ApprovalStatus, InvalidApprovalTransition> {
    match (s, e) {
        (ApprovalStatus::Pending, ApprovalEvent::Allow) => Ok(ApprovalStatus::Allowed),
        (ApprovalStatus::Pending, ApprovalEvent::Deny) => Ok(ApprovalStatus::Denied),
        (ApprovalStatus::Pending, ApprovalEvent::Expire) => Ok(ApprovalStatus::Expired),
        (ApprovalStatus::Pending, ApprovalEvent::Cancel) => Ok(ApprovalStatus::Cancelled),
        _ => Err(InvalidApprovalTransition(s, e)),
    }
}

/// Serializable view of a persisted approval (API / CLI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalView {
    pub id: String,
    pub task_id: String,
    pub attempt_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub permission: String,
    pub status: ApprovalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub created_at: String,
    pub expires_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    /// What the operator is approving: tool_call | session | step | command | duration.
    #[serde(default = "default_approval_scope")]
    pub scope: String,
}

fn default_approval_scope() -> String {
    "session".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_accepts_all_events() {
        assert_eq!(
            next_approval(ApprovalStatus::Pending, ApprovalEvent::Allow).unwrap(),
            ApprovalStatus::Allowed
        );
        assert_eq!(
            next_approval(ApprovalStatus::Pending, ApprovalEvent::Deny).unwrap(),
            ApprovalStatus::Denied
        );
        assert_eq!(
            next_approval(ApprovalStatus::Pending, ApprovalEvent::Expire).unwrap(),
            ApprovalStatus::Expired
        );
        assert_eq!(
            next_approval(ApprovalStatus::Pending, ApprovalEvent::Cancel).unwrap(),
            ApprovalStatus::Cancelled
        );
    }

    #[test]
    fn terminal_states_reject_transitions() {
        for s in [
            ApprovalStatus::Allowed,
            ApprovalStatus::Denied,
            ApprovalStatus::Expired,
            ApprovalStatus::Cancelled,
        ] {
            assert!(next_approval(s, ApprovalEvent::Allow).is_err());
            assert!(next_approval(s, ApprovalEvent::Deny).is_err());
        }
    }
}
