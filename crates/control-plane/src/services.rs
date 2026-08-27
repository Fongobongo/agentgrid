//! Service layer (plans 534/533/535): business workflows that coordinate
//! several store aggregates, so handlers stay thin (one call) and
//! cross-aggregate rules live in one testable place. Added per real
//! multi-store scenario:
//! - [`TaskLifecycleService`] (534): completing an attempt must also advance
//!   the owning workflow run + wake the scheduler.
//! - [`SchedulerService`] (533): a node poll is degrade + touch + assign, all
//!   three store operations coordinated in one request.
//! - [`ArtifactService`] (535): upload is auth + quota + write + metadata.

use std::sync::Arc;
use std::time::Instant;

use agentgrid_common::{CompleteAttemptRequest, PollRequest};
use tokio::sync::Notify;

use crate::store::{is_safe_artifact_name, Store};
use crate::POLL_TIMEOUT;

/// Attempt lifecycle orchestration: completing an attempt is atomic in the
/// store, but a completed task that belongs to a workflow run must also
/// advance the run's DAG (spawning successor steps) and wake the scheduler so
/// freshly-created step tasks get assigned promptly. Previously this only
/// happened via a manual `POST /v1/workflow_runs/{id}/tick`, so a run stalled
/// after its first step finished.
pub struct TaskLifecycleService {
    store: Store,
    assignment_notify: Arc<Notify>,
    /// Plan 1.2 (competitor #22a): webhook for terminal task events.
    notify_webhook: Option<String>,
}

impl TaskLifecycleService {
    pub fn new(
        store: Store,
        assignment_notify: Arc<Notify>,
        notify_webhook: Option<String>,
    ) -> Self {
        Self {
            store,
            assignment_notify,
            notify_webhook,
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
        // Plan 1.2 (competitor #22a): notify the operator on terminal states
        // (mobile-friendly push). Best-effort; failures are logged inside
        // `notify_task` and never abort the completion path.
        if let Some(url) = &self.notify_webhook {
            if let Ok(Some(task)) = self.store.show_task(&task_id).await {
                use agentgrid_common::TaskStatus::*;
                // Plan 1.1 patch-review: a success creates a pending
                // task_patch_review approval (operator must ack the patch).
                // Surface that as `awaiting_review` for the webhook (competitor
                // #22a) instead of `completed` so the operator gets a push
                // to review, not a "done" notification.
                let awaiting = self
                    .store
                    .find_pending_patch_review(&task_id)
                    .await
                    .unwrap_or(None)
                    .is_some();
                let status_str = match task.status {
                    Succeeded => Some(if awaiting {
                        "awaiting_review"
                    } else {
                        "completed"
                    }),
                    Failed => Some("failed"),
                    _ => None,
                };
                if let Some(s) = status_str {
                    let url = url.clone();
                    let note = crate::notify::TaskNotification {
                        task_id: task_id.clone(),
                        attempt_id: attempt_id.to_string(),
                        status: s.to_string(),
                        url: format!("/tasks/{task_id}"),
                    };
                    tokio::spawn(async move { crate::notify::notify_task(&url, &note).await });
                }
            }
        }
        // Plan 2.9 (#20): consensus collapse — when the task was part of a
        // consensus batch and every member task has reached a terminal state,
        // reconcile patch SHAs and emit a human-review approval on
        // disagreement. Idempotent, runs once per member that lands its
        // completion.
        if let Err(e) = self.store.maybe_collapse_consensus(&task_id).await {
            tracing::warn!(task_id, error = %e, "consensus collapse failed");
        }
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

/// Scheduler poll orchestration (plan 533): one node poll is three store
/// operations — mark degraded on protocol mismatch, register/touch the node
/// heartbeat, then try to assign a task (waiting on wake notifications up to
/// the poll timeout). Moving them out of the route handler makes the poll
/// contract testable in isolation and keeps the handler thin.
pub struct SchedulerService {
    store: Store,
    assignment_notify: Arc<Notify>,
}

impl SchedulerService {
    pub fn new(store: Store, assignment_notify: Arc<Notify>) -> Self {
        Self {
            store,
            assignment_notify,
        }
    }

    /// Serve one long-poll. The authenticated node id is the source of truth;
    /// a client-supplied id is ignored (the handler stamps `req.node_id`).
    /// Returns `(degraded, assignments)`: `degraded` is true when the node's
    /// protocol was rejected as incompatible (the node marks itself degraded).
    /// Plan 0.3 1.2: `max_batch` (from the opt-in `x-agentgrid-max-batch`
    /// header, 1 for legacy nodes) lets one poll fill several free slots in a
    /// single write transaction.
    pub async fn poll(
        &self,
        req: &PollRequest,
        max_batch: usize,
    ) -> anyhow::Result<(bool, Vec<agentgrid_common::Assignment>)> {
        let mut degraded = false;
        if agentgrid_common::is_incompatible_protocol(&req.protocol_version) {
            self.store.set_node_degraded(&req.node_id).await?;
            degraded = true;
        }
        self.store.register_or_touch_node(req).await?;

        let deadline = Instant::now() + POLL_TIMEOUT;
        loop {
            // Construct the waiter before checking for work: Notify's permit
            // captures a notify that lands between try_assign returning None
            // and the select below (otherwise the node idled a full poll
            // timeout despite work being assigned).
            let notified = self.assignment_notify.notified();
            tokio::pin!(notified);
            let cap = max_batch.max(1).min(req.max_concurrency as usize);
            match self.store.try_assign_batch(&req.node_id, cap).await {
                Ok(batch) if !batch.is_empty() => return Ok((degraded, batch)),
                Ok(_) => {}
                Err(e) => return Err(e),
            }
            if Instant::now() >= deadline {
                return Ok((degraded, Vec::new()));
            }
            let remaining = deadline - Instant::now();
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep(remaining) => {}
            }
        }
    }
}

/// Artifact authorization/storage orchestration (plan 535): one upload is
/// name safety + attempt-owner auth + fencing + quota + save; one read is
/// name safety + producer authorization + bytes + metadata. Coordinating them
/// in the service (instead of the route handler) keeps the handler thin and
/// the auth+quota+storage rules in one testable place. The service takes
/// `&AppState` per call (metrics counters + limits live there) and holds no
/// state of its own, so it cannot borrow-cycle with `AppState`.
pub struct ArtifactService;

/// One artifact upload: caller node, target attempt, fencing token, name,
/// body, optional media type and optional sha256 hint. Bundled so the service
/// method stays under clippy's argument-count ceiling.
pub struct UploadArtifact<'a> {
    pub node_id: &'a str,
    pub attempt_id: &'a str,
    pub fencing: Option<&'a str>,
    pub name: &'a str,
    pub bytes: &'a [u8],
    pub media_type: Option<&'a str>,
    pub sha256: Option<&'a str>,
}

/// Errors an artifact operation can fail with, before HTTP mapping.
pub enum ArtifactError {
    /// Unsafe name (traversal / control chars) or invalid attempt id.
    BadName,
    /// Attempt does not exist (or not owned by the caller).
    NotFound,
    /// Owned by another node (attempt owner mismatch).
    Forbidden,
    /// Fencing token mismatch (reassigned/lost attempt).
    StaleFence,
    /// Body over the configured artifact size limit.
    TooLarge,
    /// Storage quota would be exceeded.
    InsufficientStorage,
    /// Supplied sha256 does not match the computed hash.
    HashMismatch,
    /// Storage/db failure.
    Internal,
}

impl From<ArtifactError> for axum::http::StatusCode {
    fn from(e: ArtifactError) -> Self {
        match e {
            ArtifactError::BadName => Self::BAD_REQUEST,
            ArtifactError::NotFound => Self::NOT_FOUND,
            ArtifactError::Forbidden => Self::FORBIDDEN,
            ArtifactError::StaleFence => Self::CONFLICT,
            ArtifactError::TooLarge => Self::PAYLOAD_TOO_LARGE,
            ArtifactError::InsufficientStorage => Self::INSUFFICIENT_STORAGE,
            ArtifactError::HashMismatch => Self::UNPROCESSABLE_ENTITY,
            ArtifactError::Internal => {
                tracing::error!("artifact operation failed");
                Self::INTERNAL_SERVER_ERROR
            }
        }
    }
}

impl ArtifactService {
    /// Store an artifact on behalf of a node, enforcing name safety, attempt
    /// ownership, fencing, size limit, and storage quota before the write.
    pub async fn upload(
        state: &crate::AppState,
        upload: UploadArtifact<'_>,
    ) -> Result<agentgrid_common::ArtifactUploadResponse, ArtifactError> {
        let UploadArtifact {
            node_id,
            attempt_id,
            fencing,
            name,
            bytes,
            media_type,
            sha256,
        } = upload;
        if !is_safe_artifact_name(name) {
            return Err(ArtifactError::BadName);
        }
        let auth = crate::auth::AuthedNode {
            node_id: node_id.to_string(),
        };
        if let Err(code) = crate::auth::check_attempt_owner(state, &auth, attempt_id).await {
            return Err(match code {
                axum::http::StatusCode::NOT_FOUND => ArtifactError::NotFound,
                axum::http::StatusCode::FORBIDDEN => ArtifactError::Forbidden,
                _ => ArtifactError::Internal,
            });
        }
        if let Err(code) = crate::auth::check_fencing_token(state, attempt_id, fencing).await {
            return Err(match code {
                axum::http::StatusCode::CONFLICT => ArtifactError::StaleFence,
                axum::http::StatusCode::NOT_FOUND => ArtifactError::NotFound,
                _ => ArtifactError::Internal,
            });
        }
        if bytes.len() > state.limits.artifact {
            return Err(ArtifactError::TooLarge);
        }
        // Hardening P1 item 15: artifact storage quota (0 = unlimited). The
        // quota is captured at startup (`Limits.artifact_quota_bytes`); a
        // storage-layer failure here must NOT read as "0 bytes used" — that
        // failed the quota check open during DB incidents, so the error
        // propagates as Internal (503) instead.
        let quota = state
            .limits
            .artifact_quota_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        if quota > 0 {
            let used = state
                .store
                .artifact_storage_bytes()
                .await
                .map_err(|_| ArtifactError::Internal)?;
            if used + bytes.len() as u64 > quota {
                return Err(ArtifactError::InsufficientStorage);
            }
        }
        state
            .store
            .save_artifact_bytes(attempt_id, name, bytes, media_type, sha256)
            .await
            .map_err(|e| match e {
                crate::store::StoreArtifactError::HashMismatch { .. } => {
                    ArtifactError::HashMismatch
                }
                crate::store::StoreArtifactError::InvalidAttemptId => ArtifactError::BadName,
                _ => ArtifactError::Internal,
            })
    }

    /// Read an artifact (user path — public download) with name safety.
    pub async fn read(
        state: &crate::AppState,
        task_id: &str,
        name: &str,
    ) -> Result<Option<(Vec<u8>, Option<agentgrid_common::ArtifactMeta>)>, ArtifactError> {
        if !is_safe_artifact_name(name) {
            return Ok(None); // 404: deny without disclosing existence
        }
        match state.store.read_artifact_bytes(task_id, name).await {
            Ok(Some(bytes)) => {
                let meta = state
                    .store
                    .read_artifact_meta(task_id, name)
                    .await
                    .ok()
                    .flatten();
                Ok(Some((bytes, meta)))
            }
            Ok(None) => Ok(None),
            Err(_) => Err(ArtifactError::Internal),
        }
    }

    /// Read an artifact on behalf of a node (workflow upstream fetch) — the
    /// caller node must be authorized to read the producer task's artifact.
    pub async fn read_node(
        state: &crate::AppState,
        node_id: &str,
        task_id: &str,
        name: &str,
    ) -> Result<Option<(Vec<u8>, Option<agentgrid_common::ArtifactMeta>)>, ArtifactError> {
        if !is_safe_artifact_name(name) {
            return Ok(None); // 404: deny without disclosing existence
        }
        let allowed = state
            .store
            .can_node_read_upstream_artifact(node_id, task_id)
            .await
            .map_err(|_| ArtifactError::Internal)?;
        if !allowed {
            return Ok(None); // 404 on denial, same as the old handler
        }
        Self::read(state, task_id, name).await
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
        let p = std::env::temp_dir().join(format!("ag-svc-{nanos}-{n}.db"));
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

    /// Competitor #22a: in-process loopback HTTP server that records the
    /// body of the last POST it received. Returns `(url, last_body)`.
    async fn mock_webhook() -> (String, std::sync::Arc<tokio::sync::Mutex<Option<Vec<u8>>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");
        let last = std::sync::Arc::new(tokio::sync::Mutex::new(None::<Vec<u8>>));
        let last_c = last.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let last = last_c.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let body = if let Some(i) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
                        buf[i + 4..n].to_vec()
                    } else {
                        vec![]
                    };
                    if let Ok(mut g) = last.try_lock() {
                        *g = Some(body);
                    }
                    sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await
                        .ok();
                });
            }
        });
        (url, last)
    }

    async fn wait_for_body(last: &std::sync::Arc<tokio::sync::Mutex<Option<Vec<u8>>>>) -> String {
        for _ in 0..50 {
            if let Some(b) = last.lock().await.as_ref() {
                return String::from_utf8_lossy(b).to_string();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("webhook never received a POST");
    }

    /// Plan 534 regression: completing a step's attempt must advance the
    /// owning workflow run automatically — no manual `/tick` required. Before
    /// the service layer, a run stalled after its first step finished until an
    /// operator ticked it manually.
    #[tokio::test]
    async fn completion_advances_workflow_run_without_manual_tick() {
        let s = temp_store().await;
        let svc = TaskLifecycleService::new(s.clone(), Arc::new(Notify::new()), None);
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

    /// Plan 533: a node poll with an incompatible protocol is served with the
    /// `degraded` flag set (the service marks the node degraded, touches the
    /// heartbeat, and returns no assignment).
    #[tokio::test]
    async fn scheduler_poll_marks_degraded_on_protocol_mismatch() {
        let s = temp_store().await;
        let svc = SchedulerService::new(s.clone(), Arc::new(Notify::new()));
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
        s.enroll_node(&node).await.unwrap().expect("enroll");
        // Incompatible protocol version -> degraded, no assignment.
        let req = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: Some("999.0.0".into()),
        };
        let (degraded, assignments) = svc.poll(&req, 1).await.unwrap();
        assert!(degraded, "incompatible protocol must degrade the node");
        assert!(assignments.is_empty());
    }

    /// Competitor #22a: a successful attempt that created a pending
    /// patch-review approval must notify the webhook with status
    /// `awaiting_review` (not `completed`). Proves the notify wiring end-to-
    /// end against an in-process loopback server.
    #[tokio::test]
    async fn completion_with_pending_patch_review_notifies_awaiting_review() {
        let s = temp_store().await;
        let (url, last) = mock_webhook().await;
        let svc = TaskLifecycleService::new(s.clone(), Arc::new(Notify::new()), Some(url));

        // Plain task (not workflow) so nothing else fires.
        let t = s
            .create_task(&agentgrid_common::CreateTaskRequest {
                prompt: "build it".into(),
                repository: "demo".into(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: None,
                validation_command: None,
                base_commit: None,
                parent_acp_session_id: None,
                security_profile: None,
                network_mode: None,
                group_id: None,
                agent_id: None,
                consensus_group_id: None,
                consensus_member: None,
                opencode_override: None,
                github_push: false,
                github_repo: None,
                github_issue: None,
                github_base_ref: None,
            })
            .await
            .unwrap();
        // Enroll + assign the new task (enroll_and_assign creates its own task).
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
        let assign = s.try_assign(&node_id).await.unwrap().expect("assign");
        assert_eq!(assign.task_id, t.id);
        s.ack_attempt(&assign.attempt_id).await.unwrap();

        // Success + a changes.patch -> plan 1.1 creates a pending review.
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
        assert!(svc
            .complete_attempt(&assign.attempt_id, &req)
            .await
            .unwrap());

        // Webhook must receive awaiting_review (review exists).
        let body = wait_for_body(&last).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["task_id"], t.id);
        assert_eq!(v["status"], "awaiting_review");
    }

    /// Competitor #22a: a failed attempt must notify the webhook with
    /// status `failed`.
    #[tokio::test]
    async fn failed_completion_notifies_failed() {
        let s = temp_store().await;
        let (url, last) = mock_webhook().await;
        let svc = TaskLifecycleService::new(s.clone(), Arc::new(Notify::new()), Some(url));

        let t = s
            .create_task(&agentgrid_common::CreateTaskRequest {
                prompt: "boom".into(),
                repository: "demo".into(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: None,
                validation_command: None,
                base_commit: None,
                parent_acp_session_id: None,
                security_profile: None,
                network_mode: None,
                group_id: None,
                agent_id: None,
                consensus_group_id: None,
                consensus_member: None,
                opencode_override: None,
                github_push: false,
                github_repo: None,
                github_issue: None,
                github_base_ref: None,
            })
            .await
            .unwrap();
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
        let assign = s.try_assign(&node_id).await.unwrap().expect("assign");
        assert_eq!(assign.task_id, t.id);
        s.ack_attempt(&assign.attempt_id).await.unwrap();

        let req = CompleteAttemptRequest {
            exit_code: 1, // non-zero => failure
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
        assert!(svc
            .complete_attempt(&assign.attempt_id, &req)
            .await
            .unwrap());

        let body = wait_for_body(&last).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "failed");
    }
}
