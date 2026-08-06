//! Service layer (plan 534): business workflows that coordinate several store
//! aggregates, so handlers stay thin (one call) and cross-aggregate rules live
//! in one testable place. Only added when a real multi-store scenario exists —
//! see [`TaskLifecycleService`], the first one.

use std::sync::Arc;

use agentgrid_common::CompleteAttemptRequest;
use tokio::sync::Notify;

use crate::store::Store;

/// Attempt lifecycle orchestration: completing an attempt is atomic in the
/// store, but a completed task that belongs to a workflow run must also
/// advance the run's DAG (spawning successor steps) and wake the scheduler so
/// freshly-created step tasks get assigned promptly. Previously this only
/// happened via a manual `POST /v1/workflow_runs/{id}/tick`, so a run stalled
/// after its first step finished.
pub struct TaskLifecycleService {
    store: Store,
    assignment_notify: Arc<Notify>,
}

impl TaskLifecycleService {
    pub fn new(store: Store, assignment_notify: Arc<Notify>) -> Self {
        Self {
            store,
            assignment_notify,
        }
    }

    /// Complete an attempt, then advance the owning workflow run (if any) and
    /// wake the scheduler for tasks it spawned. Returns `false` if the attempt
    /// does not exist; the workflow advance is best-effort (a failure here
    /// must not roll back the terminal completion — the run is still tickable
    /// manually).
    pub async fn complete_attempt(
        &self,
        attempt_id: &str,
        req: &CompleteAttemptRequest,
    ) -> anyhow::Result<bool> {
        let ok = self.store.complete_attempt(attempt_id, req).await?;
        if !ok {
            return Ok(false);
        }
        let task_id = match self.store.attempt_task_id(attempt_id).await? {
            Some(t) => t,
            None => return Ok(true), // orphaned attempt; nothing to advance
        };
        let Some(run_id) = self.store.workflow_run_id_for_task(&task_id).await? else {
            return Ok(true); // plain (non-workflow) task
        };

        // Advance the DAG: successor steps whose deps are now satisfied spawn
        // tasks. Wake the scheduler so they assign without waiting for a poll.
        match self.store.tick_workflow_run(&run_id).await {
            Ok(created) if !created.is_empty() => {
                self.assignment_notify.notify_waiters();
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(
                run_id,
                attempt_id,
                error = %e,
                "workflow run advance after completion failed (run stays tickable manually)"
            ),
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentgrid_common::{
        CompleteAttemptRequest, EnrollRequest, WorkflowRole, WorkflowRunStatus, WorkflowStep,
        WorkflowStepStatus,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    async fn temp_store() -> Store {
        std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::path::Path::new("/var/tmp").join(format!("ag-svc-{nanos}-{n}.db"));
        let _ = std::fs::remove_file(&p);
        // NB: do NOT remove the file after open — the WAL pool can open a
        // second connection after the unlink, which would create a fresh empty
        // DB without the migrated tables. /var/tmp is tmpfs-cleanable.
        Store::open(p.to_str().unwrap()).await.unwrap()
    }

    fn step(id: &str, deps: &[&str], role: WorkflowRole) -> WorkflowStep {
        WorkflowStep {
            id: id.into(),
            prompt: format!("do {id}"),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            role,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: None,
            max_attempts: None,
            expandable: None,
        }
    }

    async fn enroll_and_assign(s: &Store) -> (String, String, String) {
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node = EnrollRequest {
            token,
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            agent_version: "test".into(),
            protocol_version: None,
            permission_interception: "wrapper".into(),
        };
        let node_id = s.enroll_node(&node).await.unwrap().expect("enroll").node_id;
        let a = s.try_assign(&node_id).await.unwrap().expect("assign");
        let task_id = a.task_id.clone();
        s.ack_attempt(&a.attempt_id).await.unwrap();
        (node_id, task_id, a.attempt_id)
    }

    /// Plan 534 regression: completing a step's attempt must advance the
    /// owning workflow run automatically — no manual `/tick` required. Before
    /// the service layer, a run stalled after its first step finished until an
    /// operator ticked it manually.
    #[tokio::test]
    async fn completion_advances_workflow_run_without_manual_tick() {
        let s = temp_store().await;
        let svc = TaskLifecycleService::new(s.clone(), Arc::new(Notify::new()));
        // b depends on a: after a completes, tick must activate b.
        let tpl = s
            .create_workflow_template(
                "adv",
                &[
                    step("a", &[], WorkflowRole::Worker),
                    step("b", &["a"], WorkflowRole::Worker),
                ],
                &None,
            )
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let spawned = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(spawned.len(), 1, "first tick spawns only step a");

        let (node_id, task_id, attempt_id) = enroll_and_assign(&s).await;
        assert_eq!(task_id, spawned[0], "assigned task is step a's task");
        let _ = node_id;

        let req = CompleteAttemptRequest {
            exit_code: 0,
            commit_sha: None,
            error_code: None,
            resolved_base_sha: None,
            remote_head_at_start: None,
            remote_head_at_finish: None,
            acp_session_id: None,
            provenance: None,
            plan: None,
            pending_artifacts: vec![],
        };
        assert!(svc.complete_attempt(&attempt_id, &req).await.unwrap());

        // Service must have advanced the run: step b is now running (its task
        // spawned) without any manual tick.
        let steps = s.get_workflow_run_steps(&run.id).await.unwrap();
        let b = steps.iter().find(|x| x.step_id == "b").expect("step b");
        assert_eq!(b.status, WorkflowStepStatus::Running, "b auto-activated");
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            run_got.status,
            WorkflowRunStatus::Running,
            "run still running (b pending)"
        );
    }
}
