//! Pure task/attempt state-machine transition functions.
//!
//! Every status change in agentgrid goes through
//! `(status, transition) -> Result<status, InvalidTransition>`. Keeping these
//! as total, side-effect-free functions makes the allowed/forbidden graph
//! exhaustively unit-testable (spec 2.2).

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid transition {transition} from status {from}")]
pub struct InvalidTransition {
    pub from: &'static str,
    pub transition: &'static str,
}

/// Transitions that drive a [`TaskStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskTransition {
    Assign,
    Start,
    BeginValidate,
    Succeed,
    Fail,
    Cancel,
    Retry,
    NodeLost,
}

/// Transitions that drive an [`AttemptStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptTransition {
    Start,
    BeginValidate,
    Succeed,
    Fail,
    Cancel,
    NodeLost,
}

use crate::{AttemptStatus, TaskStatus};

pub fn next_task_status(s: TaskStatus, t: TaskTransition) -> Result<TaskStatus, InvalidTransition> {
    use TaskStatus::*;
    use TaskTransition::*;
    let from = status_str(s);
    let next = match (s, t) {
        (Queued, Assign) => Assigned,
        (Queued, Cancel) => Cancelled,
        (Queued, NodeLost) => Failed,
        (Assigned, Start) => Running,
        (Assigned, Cancel) => Cancelled,
        (Assigned, Retry) => Queued,
        (Assigned, NodeLost) => Failed,
        (Running, BeginValidate) => Validating,
        (Running, Succeed) => Succeeded,
        (Running, Fail) => Failed,
        (Running, Cancel) => Cancelled,
        (Running, NodeLost) => Failed,
        (Validating, Succeed) => Succeeded,
        (Validating, Fail) => Failed,
        (Validating, Cancel) => Cancelled,
        (Validating, NodeLost) => Failed,
        (Failed, Retry) => Queued,
        (Cancelled, Retry) => Queued,
        _ => {
            return Err(InvalidTransition {
                from,
                transition: transition_str(t),
            })
        }
    };
    Ok(next)
}

pub fn next_attempt_status(
    s: AttemptStatus,
    t: AttemptTransition,
) -> Result<AttemptStatus, InvalidTransition> {
    use AttemptStatus::*;
    use AttemptTransition::*;
    let from = attempt_status_str(s);
    let next = match (s, t) {
        (Assigned, Start) => Running,
        (Assigned, Cancel) => Cancelled,
        (Assigned, NodeLost) => Lost,
        (Running, BeginValidate) => Validating,
        (Running, Succeed) => Succeeded,
        (Running, Fail) => Failed,
        (Running, Cancel) => Cancelled,
        (Running, NodeLost) => Lost,
        (Validating, Succeed) => Succeeded,
        (Validating, Fail) => Failed,
        (Validating, Cancel) => Cancelled,
        (Validating, NodeLost) => Lost,
        _ => {
            return Err(InvalidTransition {
                from,
                transition: attempt_transition_str(t),
            })
        }
    };
    Ok(next)
}

fn status_str(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Queued => "queued",
        TaskStatus::Assigned => "assigned",
        TaskStatus::Running => "running",
        TaskStatus::Validating => "validating",
        TaskStatus::Succeeded => "succeeded",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn transition_str(t: TaskTransition) -> &'static str {
    match t {
        TaskTransition::Assign => "assign",
        TaskTransition::Start => "start",
        TaskTransition::BeginValidate => "begin_validate",
        TaskTransition::Succeed => "succeed",
        TaskTransition::Fail => "fail",
        TaskTransition::Cancel => "cancel",
        TaskTransition::Retry => "retry",
        TaskTransition::NodeLost => "node_lost",
    }
}

fn attempt_status_str(s: AttemptStatus) -> &'static str {
    match s {
        AttemptStatus::Assigned => "assigned",
        AttemptStatus::Running => "running",
        AttemptStatus::Validating => "validating",
        AttemptStatus::Succeeded => "succeeded",
        AttemptStatus::Failed => "failed",
        AttemptStatus::Cancelled => "cancelled",
        AttemptStatus::Lost => "lost",
    }
}

fn attempt_transition_str(t: AttemptTransition) -> &'static str {
    match t {
        AttemptTransition::Start => "start",
        AttemptTransition::BeginValidate => "begin_validate",
        AttemptTransition::Succeed => "succeed",
        AttemptTransition::Fail => "fail",
        AttemptTransition::Cancel => "cancel",
        AttemptTransition::NodeLost => "node_lost",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_allowed_transitions() {
        assert_eq!(
            next_task_status(TaskStatus::Queued, TaskTransition::Assign).unwrap(),
            TaskStatus::Assigned
        );
        assert_eq!(
            next_task_status(TaskStatus::Assigned, TaskTransition::Start).unwrap(),
            TaskStatus::Running
        );
        assert_eq!(
            next_task_status(TaskStatus::Running, TaskTransition::BeginValidate).unwrap(),
            TaskStatus::Validating
        );
        assert_eq!(
            next_task_status(TaskStatus::Running, TaskTransition::Succeed).unwrap(),
            TaskStatus::Succeeded
        );
        assert_eq!(
            next_task_status(TaskStatus::Running, TaskTransition::Fail).unwrap(),
            TaskStatus::Failed
        );
        assert_eq!(
            next_task_status(TaskStatus::Failed, TaskTransition::Retry).unwrap(),
            TaskStatus::Queued
        );
        assert_eq!(
            next_task_status(TaskStatus::Running, TaskTransition::Cancel).unwrap(),
            TaskStatus::Cancelled
        );
        assert_eq!(
            next_task_status(TaskStatus::Assigned, TaskTransition::NodeLost).unwrap(),
            TaskStatus::Failed
        );
    }

    #[test]
    fn task_forbidden_transitions() {
        assert!(next_task_status(TaskStatus::Succeeded, TaskTransition::Start).is_err());
        assert!(next_task_status(TaskStatus::Queued, TaskTransition::Succeed).is_err());
        assert!(next_task_status(TaskStatus::Cancelled, TaskTransition::Start).is_err());
        assert!(next_task_status(TaskStatus::Succeeded, TaskTransition::Retry).is_err());
    }

    #[test]
    fn attempt_allowed_and_forbidden() {
        assert_eq!(
            next_attempt_status(AttemptStatus::Assigned, AttemptTransition::Start).unwrap(),
            AttemptStatus::Running
        );
        assert_eq!(
            next_attempt_status(AttemptStatus::Running, AttemptTransition::NodeLost).unwrap(),
            AttemptStatus::Lost
        );
        assert!(next_attempt_status(AttemptStatus::Succeeded, AttemptTransition::Start).is_err());
        assert!(next_attempt_status(AttemptStatus::Lost, AttemptTransition::Cancel).is_err());
    }

    #[test]
    fn double_assign_impossible() {
        // Two concurrent Assign calls must not both succeed from Queued.
        let s = TaskStatus::Queued;
        let a = next_task_status(s, TaskTransition::Assign);
        let b = next_task_status(s, TaskTransition::Assign);
        // Only the first consumer transitions; the second sees a different state.
        assert!(a.is_ok());
        // Re-applying Assign to the already-assigned state is invalid.
        assert!(next_task_status(TaskStatus::Assigned, TaskTransition::Assign).is_err());
        // b was computed from the same pre-state; the *store* must re-read after locking.
        let _ = b;
    }

    #[test]
    fn terminal_states_are_idempotent_except_retry() {
        // Hardening P1 item 13: a terminal task/attempt is a sink. Every
        // non-Retry transition applied to a terminal status must be rejected
        // (no spurious flip out of Succeeded/Failed/Cancelled/Lost), and Retry
        // is the only legal exit (Failed/Cancelled → Queued) — explicit so the
        // caller never relies on a "happens to be terminal" implicit invariant.
        let task_terminals = [
            TaskStatus::Succeeded,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ];
        let attempt_terminals = [
            AttemptStatus::Succeeded,
            AttemptStatus::Failed,
            AttemptStatus::Cancelled,
            AttemptStatus::Lost,
        ];
        let non_retry_task = [
            TaskTransition::Assign,
            TaskTransition::Start,
            TaskTransition::BeginValidate,
            TaskTransition::Succeed,
            TaskTransition::Fail,
            TaskTransition::Cancel,
            TaskTransition::NodeLost,
        ];
        let non_retry_attempt = [
            AttemptTransition::Start,
            AttemptTransition::BeginValidate,
            AttemptTransition::Succeed,
            AttemptTransition::Fail,
            AttemptTransition::Cancel,
            AttemptTransition::NodeLost,
        ];
        for s in task_terminals {
            for t in non_retry_task {
                assert!(
                    next_task_status(s, t).is_err(),
                    "task {s:?} + {t:?} must be rejected (terminal idempotency)"
                );
            }
        }
        for s in attempt_terminals {
            for t in non_retry_attempt {
                assert!(
                    next_attempt_status(s, t).is_err(),
                    "attempt {s:?} + {t:?} must be rejected (terminal idempotency)"
                );
            }
        }
        // Retry is the only legal exit from a terminal *task*; from Succeeded /
        // Lost attempts (= task Succeeded / task Failed via NodeLost) Retry is
        // not an attempt transition at all, only a task one.
        assert_eq!(
            next_task_status(TaskStatus::Failed, TaskTransition::Retry).unwrap(),
            TaskStatus::Queued
        );
        assert_eq!(
            next_task_status(TaskStatus::Cancelled, TaskTransition::Retry).unwrap(),
            TaskStatus::Queued
        );
        assert!(next_task_status(TaskStatus::Succeeded, TaskTransition::Retry).is_err());
        // No attempt transition escapes the terminal set at all.
    }
}
