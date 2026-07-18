//! Durable approval state machine (Stage 5 prerequisite, built before any ACP
//! integration). The DB table + API + CLI wiring lives in the control plane;
//! this is the pure transition core that the durable store and the
//! `session/request_permission` path both route through.
//!
//! Fail-closed by policy: a `Pending` approval that is not explicitly `Allow`ed
//! ends `Denied`/`Expired` — there is no unconditional-allow shortcut.

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalStatus {
    Pending,
    Allowed,
    Denied,
    Expired,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
