use crate::helpers::*;

#[tokio::test]
async fn cancel_queued_marks_cancelled() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let req = CreateTaskRequest {
        prompt: "x".into(),
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
        github_create: false,
        max_attempts: 1,
        consensus_mode: None,
        review_of: None,
        max_tokens: None,
        max_cost_cents: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/tasks",
            serde_json::to_string(&req).unwrap(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let tv: TaskView = serde_json::from_slice(&body).unwrap();

    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/tasks/{}/cancel", tv.id),
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(show_status(&app, &tv.id).await, TaskStatus::Cancelled);
}

#[tokio::test]
async fn cancel_running_then_node_confirms_cancelled() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-c", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "sleep:30").await;
    // Acknowledge the assignment before completing.
    ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await;

    let cs: CancelState = cancel_state(&app, &assign.attempt_id, &cred).await;
    assert!(!cs.cancel_requested);

    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/tasks/{}/cancel", assign.task_id),
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let cs: CancelState = cancel_state(&app, &assign.attempt_id, &cred).await;
    assert!(cs.cancel_requested);

    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            })
            .unwrap(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Cancelled
    );
}

#[tokio::test]
async fn retry_failed_task_reques() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-r", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "fail:3").await;
    // Acknowledge the assignment before completing.
    ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await;
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 3,
                commit_sha: None,
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            })
            .unwrap(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Failed);

    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/tasks/{}/retry", assign.task_id),
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Queued);
}

/// Stage 2.4: no registered nodes => the task reports why it stays queued.
#[tokio::test]
async fn eligibility_empty_pool() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let id = create_task_only(&app, "demo", "mock", None).await;
    let elig = task_eligibility(&app, &id).await;
    assert!(!elig.nodes.iter().any(|n| n.eligible));
    assert_eq!(elig.no_eligible_nodes, vec!["no nodes registered"]);
}

/// Stage 2.4: missing adapter filter is reported per node.
#[tokio::test]
async fn eligibility_missing_adapter() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, _cred) = enroll(&app, "n", vec!["codex".into()], vec!["*".into()]).await;
    let id = create_task_only(&app, "demo", "mock", None).await;
    let elig = task_eligibility(&app, &id).await;
    let n = elig.nodes.iter().find(|n| n.node_id == node_id).unwrap();
    assert!(!n.eligible);
    assert!(n.reasons.iter().any(|r| r == "missing adapter mock"));
    assert_eq!(elig.no_eligible_nodes, vec!["missing adapter mock"]);
}

/// Stage 2.4: missing repository filter is reported per node.
#[tokio::test]
async fn eligibility_missing_repository() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, _cred) = enroll(&app, "n", vec!["mock".into()], vec!["other".into()]).await;
    let id = create_task_only(&app, "demo", "mock", None).await;
    let elig = task_eligibility(&app, &id).await;
    let n = elig.nodes.iter().find(|n| n.node_id == node_id).unwrap();
    assert!(!n.eligible);
    assert!(n.reasons.iter().any(|r| r == "missing repository demo"));
}

/// Stage 2.4: at-capacity node is reported and not eligible.
#[tokio::test]
async fn eligibility_at_capacity() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, _cred) = enroll(&app, "n", vec!["mock".into()], vec!["*".into()]).await;
    // Drain the node's capacity (max_concurrency=2) with two running attempts.
    for _ in 0..2 {
        let a = create_and_assign(&app, &node_id, &_cred, "sleep:30").await;
        let ev = IngestEventsRequest {
            events: vec![IncomingEvent {
                sequence: 1,
                r#type: EventType::Stdout,
                payload: json!({"text": "x"}),
            }],
        };
        app.clone()
            .oneshot(post_node(
                &format!("/v1/node/attempts/{}/events", a.attempt_id),
                serde_json::to_string(&ev).unwrap(),
                &_cred,
                &a.fencing_token,
            ))
            .await
            .unwrap();
    }
    let id = create_task_only(&app, "demo", "mock", None).await;
    let elig = task_eligibility(&app, &id).await;
    let n = elig.nodes.iter().find(|n| n.node_id == node_id).unwrap();
    assert!(!n.eligible);
    assert!(n.reasons.iter().any(|r| r.starts_with("at capacity")));
}

/// Stage 2.4: requested_node_id restricts eligibility to that node, and a
/// missing/offline requested node yields a clear reason.
#[tokio::test]
async fn eligibility_requested_node() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (_node_id, _cred) = enroll(&app, "n", vec!["mock".into()], vec!["*".into()]).await;

    // Request a node that is not registered: only it is considered.
    let id = create_task_only(&app, "demo", "mock", Some("ghost".into())).await;
    let elig = task_eligibility(&app, &id).await;
    assert!(elig.nodes.is_empty());
    assert_eq!(
        elig.no_eligible_nodes,
        vec!["requested node ghost not registered"]
    );

    // Request an actual eligible node: eligible, no reasons.
    let (good, _c) = enroll(&app, "good", vec!["mock".into()], vec!["*".into()]).await;
    let id2 = create_task_only(&app, "demo", "mock", Some(good.clone())).await;
    let elig2 = task_eligibility(&app, &id2).await;
    assert_eq!(elig2.nodes.len(), 1);
    assert!(elig2.nodes[0].eligible);
    assert!(elig2.no_eligible_nodes.is_empty());
}

#[tokio::test]
async fn scheduler_skips_incompatible_head_of_line() {
    // Stage 1.4: an older queued task the node cannot run (wrong adapter) must
    // not block a newer compatible one.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (_claude_node, _cc) =
        enroll(&app, "n-claude", vec!["claude".into()], vec!["*".into()]).await;
    let (mock_node, mock_cred) =
        enroll(&app, "n-mock", vec!["mock".into()], vec!["*".into()]).await;

    // Older queued task needs claude; a newer one needs mock.
    let claude_task = create_task(&app, "claude", None).await;
    let mock_task = create_task(&app, "mock", None).await;

    // mock node polls: must skip the claude head-of-line and take the mock task.
    let mut got = None;
    for _ in 0..50 {
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/v1/node/poll",
                serde_json::to_string(&PollRequest {
                    node_id: mock_node.clone(),
                    name: "n-mock".into(),
                    adapters: vec!["mock".into()],
                    repositories: vec!["*".into()],
                    max_concurrency: 2,
                    protocol_version: None,
                })
                .unwrap(),
                &mock_cred,
            ))
            .await
            .unwrap();
        let pr: PollResponse =
            serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        if let Some(a) = pr.assignment {
            got = Some(a);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let got = got.expect("mock node got no assignment");
    assert_eq!(got.task_id, mock_task);
    assert_ne!(got.task_id, claude_task);
}

#[tokio::test]
async fn scheduler_respects_requested_node() {
    // Stage 1.4: a task pinned to one node must not be assigned to another.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_a, cred_a) = enroll(&app, "n-a", vec!["mock".into()], vec!["*".into()]).await;
    let (node_b, cred_b) = enroll(&app, "n-b", vec!["mock".into()], vec!["*".into()]).await;

    let pinned = create_task(&app, "mock", Some(&node_a)).await;

    // node_b polls: must NOT get the pinned task.
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/poll",
            serde_json::to_string(&PollRequest {
                node_id: node_b.clone(),
                name: "n-b".into(),
                adapters: vec!["mock".into()],
                repositories: vec!["*".into()],
                max_concurrency: 2,
                protocol_version: None,
            })
            .unwrap(),
            &cred_b,
        ))
        .await
        .unwrap();
    let pr: PollResponse =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(
        pr.assignment.is_none(),
        "pinned task leaked to non-requested node"
    );

    // node_a polls: gets it.
    let mut got = None;
    for _ in 0..50 {
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/v1/node/poll",
                serde_json::to_string(&PollRequest {
                    node_id: node_a.clone(),
                    name: "n-a".into(),
                    adapters: vec!["mock".into()],
                    repositories: vec!["*".into()],
                    max_concurrency: 2,
                    protocol_version: None,
                })
                .unwrap(),
                &cred_a,
            ))
            .await
            .unwrap();
        let pr: PollResponse =
            serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        if let Some(a) = pr.assignment {
            got = Some(a);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let got = got.expect("requested node got no assignment");
    assert_eq!(got.task_id, pinned);
}

/// Acknowledge an assignment via the explicit ack endpoint.

#[tokio::test]
async fn ack_attempt_moves_to_running() {
    // Stage 1.3: explicit ack flips the assigned attempt (and task) to running.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-ack3", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Assigned
    );
    assert_eq!(
        ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await,
        StatusCode::OK
    );
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Running
    );
    // Idempotent re-ack.
    assert_eq!(
        ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await,
        StatusCode::OK
    );
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Running
    );
}

#[tokio::test]
async fn unacked_assignment_is_reverted() {
    // Stage 1.3: a node that never acks loses the assignment once the ack
    // deadline passes; the task returns to the queue.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-ack1", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    state
        .store
        .set_attempt_ack_deadline(&assign.attempt_id, "1970-01-01T00:00:00Z")
        .await
        .unwrap();
    state.store.tick_maintenance().await.unwrap();
    assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Queued);
}

#[tokio::test]
async fn acked_slow_agent_keeps_assignment() {
    // Stage 1.3: after ack, a slow agent that produces no output for >deadline
    // seconds must NOT lose the assignment.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-ack2", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    assert_eq!(
        ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await,
        StatusCode::OK
    );
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Running
    );
    // Force the ack deadline into the past and run maintenance: still running.
    state
        .store
        .set_attempt_ack_deadline(&assign.attempt_id, "1970-01-01T00:00:00Z")
        .await
        .unwrap();
    state.store.tick_maintenance().await.unwrap();
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Running
    );
}

/// Hardening P0 item 7 (lease/ACK race): a lease that has expired MUST be
/// reverted to the queue with the concurrency counter decremented exactly
/// once, and a late ACK arriving afterward MUST be idempotent (not flip back
/// to running, not double-decrement the node).
#[tokio::test]
async fn race_ack_after_lease_expiry_is_idempotent() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-race1", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    // Expire the ack deadline, then run the maintenance sweep that reverts it.
    state
        .store
        .set_attempt_ack_deadline(&assign.attempt_id, "1970-01-01T00:00:00Z")
        .await
        .unwrap();
    state.store.tick_maintenance().await.unwrap();
    assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Queued);
    // Capacity was freed exactly once.
    let aa: i64 = sqlx::query_scalar("SELECT active_attempts FROM nodes WHERE id = ?")
        .bind(&node_id)
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(aa, 0, "active_attempts decremented once by lease revert");
    // Late ACK for the reverted attempt is REJECTED, not acknowledged: the
    // revert rotated the fencing token, so the stale holder's presentation
    // 409s at the fencing check (a 404 from the store-level check would be
    // equivalent). Either way the task must NOT flip back to running and the
    // counter must NOT decrement again — and the fixed node drops the
    // assignment on 404/409 instead of running the agent.
    assert_eq!(
        ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await,
        StatusCode::CONFLICT
    );
    assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Queued);
    let aa2: i64 = sqlx::query_scalar("SELECT active_attempts FROM nodes WHERE id = ?")
        .bind(&node_id)
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(aa2, 0, "late ack must not double-decrement");
}

/// Hardening P0 item 7 (lease/ACK race): ACK racing WITH the lease sweep must
/// settle in ONE terminal transition — the attempt is exactly `running` and
/// the concurrency counter is exactly 1 after both have run.
#[tokio::test]
async fn race_ack_and_lease_settle_once() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-race2", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    // ACK first (wins the CAS).
    assert_eq!(
        ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await,
        StatusCode::OK
    );
    // Expire the (already-superseded) ack deadline and run the sweep: it must
    // find status != 'assigned' and leave the running attempt + counter alone.
    state
        .store
        .set_attempt_ack_deadline(&assign.attempt_id, "1970-01-01T00:00:00Z")
        .await
        .unwrap();
    state.store.tick_maintenance().await.unwrap();
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Running
    );
    let aa: i64 = sqlx::query_scalar("SELECT active_attempts FROM nodes WHERE id = ?")
        .bind(&node_id)
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(
        aa, 1,
        "ack wins; lease sweep is a no-op on a running attempt"
    );
}

/// Hardening P0 item 7 (offline sweep race): a fresh heartbeat that
/// re-online a node must NOT keep it offline after a subsequent sweep (the
/// sweep CAS `online`->`offline` only fires on a stale heartbeat, so a fresh
/// heartbeat re-asserts `online` and the next sweep is a no-op).
#[tokio::test]
async fn race_fresh_heartbeat_beats_offline_sweep() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-race3", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    assert_eq!(
        ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await,
        StatusCode::OK
    );
    // Age the heartbeat so the sweep flips the node offline and loses the
    // running attempt (attempt=lost, task=failed/node_lost).
    sqlx::query("UPDATE nodes SET last_heartbeat_at = '1970-01-01T00:00:00Z' WHERE id = ?")
        .bind(&node_id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    state.store.tick_maintenance().await.unwrap();
    assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Failed);
    // A fresh heartbeat re-online the node; the very next sweep must find it
    // `online` with a recent last_heartbeat_at and leave it online (no
    // double-flip, no spurious second lose of the already-terminal attempt).
    state
        .store
        .heartbeat(
            &node_id,
            &agentgrid_common::HeartbeatRequest {
                status: Some(agentgrid_common::NodeStatus::Online),
                name: "node-race3".into(),
                adapters: vec!["mock".into()],
                repositories: vec!["*".into()],
                max_concurrency: 1,
                agent_version: "mock".into(),
                active_attempts: 0,
                load_avg: 0.0,
                cpu_count: 4,
                free_memory_mb: 100_000,
                free_disk_mb: 100_000,
                mem_available_mb: 4096,
                capabilities: vec![],
                protocol_version: None,
                discovered_skills: vec![],
                unsafe_active: false,
                permission_interception: "wrapper".into(),
                outbox_bytes: 0,
                artifact_spool_bytes: 0,
                outbox_rows: 0,
                outbox_oldest_pending_age_ms: 0,
                outbox_corruption_count: 0,
                outbox_completion_rows: 0,
                repo_lock_wait_ms: 0,
                git_lfs_installed: false,
                git_submodules_supported: true,
                sandbox_backend: "none".into(),
                enforced_limits: false,
                systemd_scope_supported: false,
                repo_states: vec![],
                repo_cache_bytes: 0,
                workspace_bytes: 0,
                network_mode: "none".into(),
                account_usage: vec![],
                applied_opencode_hash: None,
                active_rss_mib: 0,
                max_rss_mib: 0,
                capabilities_schema_version: None,
                supported_event_versions: None,
            },
        )
        .await
        .unwrap();
    state.store.tick_maintenance().await.unwrap();
    let st: String = sqlx::query_scalar("SELECT status FROM nodes WHERE id = ?")
        .bind(&node_id)
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(st, "online", "fresh heartbeat beats offline sweep");
}

#[tokio::test]
async fn complete_on_lost_attempt_is_idempotent() {
    // Stage 1.2: a node that comes back and reports a completion for an attempt
    // we already marked `lost` must not corrupt the failed task status.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-l", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;

    // Node drops offline -> attempt lost, task failed/node_lost.
    state.store.mark_node_offline(&node_id).await.unwrap();
    let tv = show_task_view(&app, &assign.task_id).await;
    assert_eq!(tv.status, TaskStatus::Failed);
    assert_eq!(tv.error_code.as_deref(), Some("node_lost"));

    // Node returns and reports a (late) completion for the lost attempt.
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
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
                validation_rounds: 0,
            })
            .unwrap(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    // Idempotent ack: terminal/lost attempt is not re-completed.
    assert_eq!(resp.status(), StatusCode::OK);

    // Task status must remain failed/node_lost (no corruption).
    let tv = show_task_view(&app, &assign.task_id).await;
    assert_eq!(tv.status, TaskStatus::Failed);
    assert_eq!(tv.error_code.as_deref(), Some("node_lost"));
}

/// Hardening P0 item 7 (cancel ↔ complete race): whichever write wins the
/// SQLite write lock first decides the terminal status — `succeeded` if the
/// node completes first, or `cancelled` if the cancel task marks
/// `cancel_requested=1` first (completion then resolves to `cancelled`). The
/// two never both flip: the second writer observes the first writer's effect
/// and resolves to ONE terminal state. Both interleavings are exercised.
#[tokio::test]
async fn race_cancel_vs_complete_settles_once() {
    for complete_first in [true, false] {
        let state = AppState::open_temp().await.unwrap();
        let app = build_router(state.clone());
        let (node_id, cred) = enroll(&app, "n-cc", vec!["mock".into()], vec!["*".into()]).await;
        let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
        assert_eq!(
            ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await,
            StatusCode::OK
        );
        if complete_first {
            // Complete wins: task succeeds; later cancel is a no-op (no active attempt).
            assert!(state
                .store
                .complete_attempt(&assign.attempt_id, &complete_req())
                .await
                .unwrap());
            assert!(!(state.store.cancel_task(&assign.task_id).await.unwrap()));
            assert_eq!(
                show_status(&app, &assign.task_id).await,
                TaskStatus::Succeeded
            );
        } else {
            // Cancel wins: cancel_requested=1; completion then resolves to cancelled.
            assert!(state.store.cancel_task(&assign.task_id).await.unwrap());
            assert!(state
                .store
                .complete_attempt(&assign.attempt_id, &complete_req())
                .await
                .unwrap());
            assert_eq!(
                show_status(&app, &assign.task_id).await,
                TaskStatus::Cancelled
            );
        }
        // Invariant: exactly one terminal transition, counter reconciled.
        let aa: i64 = sqlx::query_scalar("SELECT active_attempts FROM nodes WHERE id = ?")
            .bind(&node_id)
            .fetch_one(&state.store.pool)
            .await
            .unwrap();
        assert_eq!(aa, 0, "active_attempts reconciled after single terminal");
    }
}

/// Hardening P0 item 7 (lost ↔ complete race): if completion lands first the
/// task succeeds and `mark_node_offline` finds no non-terminal attempt to
/// lose; if the node is marked offline first the attempt becomes `lost` and
/// the subsequent late completion is idempotent (no corruption).
#[tokio::test]
async fn race_lost_vs_complete_settles_once() {
    for offline_first in [true, false] {
        let state = AppState::open_temp().await.unwrap();
        let app = build_router(state.clone());
        let (node_id, cred) = enroll(&app, "n-lc", vec!["mock".into()], vec!["*".into()]).await;
        let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
        assert_eq!(
            ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await,
            StatusCode::OK
        );
        if offline_first {
            assert!(state.store.mark_node_offline(&node_id).await.unwrap());
            assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Failed);
            // Late completion for a `lost` attempt is an idempotent ack.
            assert!(state
                .store
                .complete_attempt(&assign.attempt_id, &complete_req())
                .await
                .unwrap());
            assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Failed);
        } else {
            assert!(state
                .store
                .complete_attempt(&assign.attempt_id, &complete_req())
                .await
                .unwrap());
            assert_eq!(
                show_status(&app, &assign.task_id).await,
                TaskStatus::Succeeded
            );
            // Offline sweep afterwards: no non-terminal attempt to lose.
            assert!(state.store.mark_node_offline(&node_id).await.unwrap());
            assert_eq!(
                show_status(&app, &assign.task_id).await,
                TaskStatus::Succeeded
            );
        }
        let aa: i64 = sqlx::query_scalar("SELECT active_attempts FROM nodes WHERE id = ?")
            .bind(&node_id)
            .fetch_one(&state.store.pool)
            .await
            .unwrap();
        assert_eq!(aa, 0, "active_attempts reconciled after single terminal");
    }
}

/// Hardening P0 item 7 (retry ↔ late completion race): a failed task can be
/// retried (re-queued). A late completion for the old attempt that arrives
/// after the retry must NOT flip the freshly-queued task back to a terminal
/// state — the queued task is left untouched and the old attempt stays lost
/// (idempotent terminal ack).
#[tokio::test]
async fn race_retry_vs_late_completion() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "n-rc", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "fail:3").await;
    assert_eq!(
        ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await,
        StatusCode::OK
    );
    // Attempt fails -> task failed.
    let fail_req = CompleteAttemptRequest {
        exit_code: 3,
        commit_sha: None,
        error_code: None,
        resolved_base_sha: None,
        remote_head_at_start: None,
        remote_head_at_finish: None,
        acp_session_id: None,
        provenance: None,
        plan: None,
        pending_artifacts: vec![],
        validation_rounds: 0,
    };
    assert!(state
        .store
        .complete_attempt(&assign.attempt_id, &fail_req)
        .await
        .unwrap());
    assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Failed);
    // Retry re-queues the task (assigned_attempt_id cleared).
    assert!(state.store.retry_task(&assign.task_id).await.unwrap());
    assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Queued);
    // A late completion for the old (already-failed) attempt is idempotent and
    // must NOT perturb the queued retry.
    assert!(state
        .store
        .complete_attempt(&assign.attempt_id, &complete_req())
        .await
        .unwrap());
    assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Queued);
}

/// Hardening P0 item 7: iterations of the ACK-vs-lease-expiry race with
/// an adversarial interleaving each iteration (ack first, then expire+sweep,
/// then expire-before-ack). The invariant — at most one `running` attempt and
/// `active_attempts` exactly matching live running attempts — holds every
/// time. Default 10 iterations keeps the suite fast; the nightly stress job
/// sets AGENTGRID_RACE_ITERS=100 for the full gate.
#[tokio::test]
async fn race_ack_lease_100_iterations_no_drift() {
    let iters: u32 = std::env::var("AGENTGRID_RACE_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(10);
    for _ in 0..iters {
        let state = AppState::open_temp().await.unwrap();
        let app = build_router(state.clone());
        let (node_id, cred) = enroll(&app, "n-r100", vec!["mock".into()], vec!["*".into()]).await;
        let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
        // Branch A: ack wins, lease sweep is a no-op.
        assert_eq!(
            ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await,
            StatusCode::OK
        );
        state
            .store
            .set_attempt_ack_deadline(&assign.attempt_id, "1970-01-01T00:00:00Z")
            .await
            .unwrap();
        state.store.tick_maintenance().await.unwrap();
        assert_eq!(
            show_status(&app, &assign.task_id).await,
            TaskStatus::Running
        );
        let aa: i64 = sqlx::query_scalar("SELECT active_attempts FROM nodes WHERE id = ?")
            .bind(&node_id)
            .fetch_one(&state.store.pool)
            .await
            .unwrap();
        assert_eq!(aa, 1, "ack wins -> exactly one running attempt");
    }
    // Branch B: lease wins (ack never sent), task re-queued, counter 0.
    for _ in 0..iters {
        let state = AppState::open_temp().await.unwrap();
        let app = build_router(state.clone());
        let (node_id, cred) = enroll(&app, "n-r100b", vec!["mock".into()], vec!["*".into()]).await;
        let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
        state
            .store
            .set_attempt_ack_deadline(&assign.attempt_id, "1970-01-01T00:00:00Z")
            .await
            .unwrap();
        state.store.tick_maintenance().await.unwrap();
        assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Queued);
        let aa: i64 = sqlx::query_scalar("SELECT active_attempts FROM nodes WHERE id = ?")
            .bind(&node_id)
            .fetch_one(&state.store.pool)
            .await
            .unwrap();
        assert_eq!(aa, 0, "lease wins -> no running attempt, counter 0");
    }
}

/// Hardening P1 item 13 (state-machine invariants): after every lifecycle
/// terminal the task satisfies: status is terminal, `finished_at` is set,
/// `assigned_attempt_id` is NULL, and the owning node's `active_attempts` is
/// 0. Walked for succeed, fail, cancel, and node-lost outcomes.
#[tokio::test]
async fn state_machine_terminal_invariants_hold() {
    async fn assert_terminal_invariants(
        app: &Router,
        state: &AppState,
        task_id: &str,
        node_id: &str,
    ) {
        let tv = show_task_view(app, task_id).await;
        assert!(
            matches!(
                tv.status,
                TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
            ),
            "terminal status, got {:?}",
            tv.status
        );
        assert!(
            tv.finished_at.is_some(),
            "finished_at must be set on terminal task"
        );
        assert!(
            tv.assigned_attempt_id.is_none(),
            "terminal task must have no active attempt"
        );
        let aa: i64 = sqlx::query_scalar("SELECT active_attempts FROM nodes WHERE id = ?")
            .bind(node_id)
            .fetch_one(&state.store.pool)
            .await
            .unwrap();
        assert_eq!(aa, 0, "active_attempts reconciled to 0 after terminal");
    }

    // Succeed.
    {
        let state = AppState::open_temp().await.unwrap();
        let app = build_router(state.clone());
        let (node_id, cred) = enroll(&app, "n-inv-s", vec!["mock".into()], vec!["*".into()]).await;
        let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
        assert!(
            ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token)
                .await
                .is_success()
        );
        assert!(state
            .store
            .complete_attempt(&assign.attempt_id, &complete_req())
            .await
            .unwrap());
        assert_terminal_invariants(&app, &state, &assign.task_id, &node_id).await;
    }
    // Fail.
    {
        let state = AppState::open_temp().await.unwrap();
        let app = build_router(state.clone());
        let (node_id, cred) = enroll(&app, "n-inv-f", vec!["mock".into()], vec!["*".into()]).await;
        let assign = create_and_assign(&app, &node_id, &cred, "fail:3").await;
        assert!(
            ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token)
                .await
                .is_success()
        );
        assert!(state
            .store
            .complete_attempt(
                &assign.attempt_id,
                &CompleteAttemptRequest {
                    exit_code: 3,
                    ..complete_req()
                }
            )
            .await
            .unwrap());
        assert_terminal_invariants(&app, &state, &assign.task_id, &node_id).await;
    }
    // Cancel (queued task).
    {
        let state = AppState::open_temp().await.unwrap();
        let app = build_router(state.clone());
        let (node_id, _cred) = enroll(&app, "n-inv-c", vec!["mock".into()], vec!["*".into()]).await;
        // A queued (never-assigned) task: cancel moves it straight to Cancelled.
        let task_id = create_task(&app, "mock", None).await;
        assert!(state.store.cancel_task(&task_id).await.unwrap());
        assert_terminal_invariants(&app, &state, &task_id, &node_id).await;
    }
    // Node lost.
    {
        let state = AppState::open_temp().await.unwrap();
        let app = build_router(state.clone());
        let (node_id, cred) = enroll(&app, "n-inv-l", vec!["mock".into()], vec!["*".into()]).await;
        let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
        assert!(
            ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token)
                .await
                .is_success()
        );
        assert!(state.store.mark_node_offline(&node_id).await.unwrap());
        assert_terminal_invariants(&app, &state, &assign.task_id, &node_id).await;
    }
    // Hardening P1 item 22: retry of a failed task leaves `active_attempts`
    // 0 (the task is queued but not yet reassigned), and the counter stays 0
    // while the retried task is pending, then a new assign bumps it back to 1.
    {
        let state = AppState::open_temp().await.unwrap();
        let app = build_router(state.clone());
        let (node_id, cred) = enroll(&app, "n-inv-r", vec!["mock".into()], vec!["*".into()]).await;
        let assign = create_and_assign(&app, &node_id, &cred, "fail:3").await;
        assert!(
            ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token)
                .await
                .is_success()
        );
        assert!(state
            .store
            .complete_attempt(
                &assign.attempt_id,
                &CompleteAttemptRequest {
                    exit_code: 3,
                    ..complete_req()
                }
            )
            .await
            .unwrap());
        // After fail the counter is 0 (terminal invariant already covers this).
        assert_terminal_invariants(&app, &state, &assign.task_id, &node_id).await;
        // Retry re-queues; no new attempt yet → counter still 0.
        assert!(state.store.retry_task(&assign.task_id).await.unwrap());
        let aa_retry: i64 = sqlx::query_scalar("SELECT active_attempts FROM nodes WHERE id = ?")
            .bind(&node_id)
            .fetch_one(&state.store.pool)
            .await
            .unwrap();
        assert_eq!(
            aa_retry, 0,
            "retry leaves active_attempts 0 before reassign"
        );
        // A fresh assign bumps it back to 1.
        let a2 = state.store.try_assign(&node_id).await.unwrap().unwrap();
        let aa_run: i64 = sqlx::query_scalar("SELECT active_attempts FROM nodes WHERE id = ?")
            .bind(&node_id)
            .fetch_one(&state.store.pool)
            .await
            .unwrap();
        assert_eq!(aa_run, 1, "reassign bumps active_attempts to 1");
        let _ = a2;
    }
}

#[tokio::test]
async fn complete_persists_resolved_base_sha() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "n-rbs", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    // First event -> running, so the attempt is assignable to a terminal path.
    let ev = IngestEventsRequest {
        events: vec![IncomingEvent {
            sequence: 1,
            r#type: EventType::Stdout,
            payload: json!({"text": "start"}),
        }],
    };
    let r = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/events", assign.attempt_id),
            serde_json::to_string(&ev).unwrap(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: Some("deadbeef".into()),
                error_code: None,
                resolved_base_sha: Some("BASECAFE".into()),
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            })
            .unwrap(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let base: Option<String> =
        sqlx::query_scalar("SELECT resolved_base_sha FROM attempts WHERE id = ?")
            .bind(&assign.attempt_id)
            .fetch_one(&state.store.pool)
            .await
            .unwrap();
    assert_eq!(base.as_deref(), Some("BASECAFE"));
}

/// Hardening P1 item 32: the remote HEAD captured at attempt start/finish must
/// round-trip from the completion request to the attempt row.
#[tokio::test]
async fn complete_persists_remote_head_at_start_and_finish() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "n-rh", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "echo hi").await;
    // event 1 -> running so the attempt reaches the terminal completion path.
    let ev = IngestEventsRequest {
        events: vec![IncomingEvent {
            sequence: 1,
            r#type: EventType::Stdout,
            payload: json!({"text": "start"}),
        }],
    };
    let r = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/events", assign.attempt_id),
            serde_json::to_string(&ev).unwrap(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: Some("AAA111".into()),
                remote_head_at_finish: Some("BBB222".into()),
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            })
            .unwrap(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let (start, finish): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT remote_head_at_start, remote_head_at_finish FROM attempts WHERE id = ?",
    )
    .bind(&assign.attempt_id)
    .fetch_one(&state.store.pool)
    .await
    .unwrap();
    assert_eq!(
        start.as_deref(),
        Some("AAA111"),
        "remote_head_at_start persisted"
    );
    assert_eq!(
        finish.as_deref(),
        Some("BBB222"),
        "remote_head_at_finish persisted"
    );
}

/// Hardening P0 item 12: begin_validate flips the attempt+task to `validating`;
/// a wrong fencing token is rejected with 409; a non-running attempt is a
/// harmless idempotent OK.
#[tokio::test]
async fn begin_validate_transitions_running_to_validating() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-val", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    assert_eq!(
        ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await,
        StatusCode::OK
    );
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Running
    );

    // Valid begin_validate.
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/begin_validate", assign.attempt_id),
            "{}".into(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Validating
    );

    // Idempotent: already validating -> OK (no error).
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/begin_validate", assign.attempt_id),
            "{}".into(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Wrong fencing token -> 409.
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/begin_validate", assign.attempt_id),
            "{}".into(),
            &cred,
            "stale-token",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

/// Competitor-gap feature (task-level auto-retry, hatchet-inspired): with
/// `max_attempts = 2` a failed attempt re-queues the task automatically; the
/// second failure exhausts the budget and the task stays failed. A cancelled
/// attempt is never auto-retried.
#[tokio::test]
async fn task_auto_retry_requeues_until_budget_exhausted() {
    let state = AppState::open_temp().await.unwrap();
    state
        .store
        .create_repository(&agentgrid_common::CreateRepositoryRequest {
            name: "demo".into(),
            git_url: "https://example.com/demo.git".into(),
            default_branch: "main".into(),
            validation_command: None,
            requirements: None,
        })
        .await
        .unwrap();
    let app = build_router(state.clone());
    let tok = test_token(&app).await;
    let (node_id, _cred) = enroll(&app, "n-retry", vec!["mock".into()], vec!["demo".into()]).await;
    let task = state
        .store
        .create_task(&agentgrid_common::CreateTaskRequest {
            prompt: "auto-retry fixture".into(),
            repository: "demo".into(),
            adapter: "mock".into(),
            max_attempts: 2,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(task.max_attempts, 2, "TaskView must echo max_attempts");

    // Attempt 1 fails -> budget remains -> task re-queued automatically.
    let a1 = state.store.try_assign(&node_id).await.unwrap().unwrap();
    state.store.ack_attempt(&a1.attempt_id).await.unwrap();
    state
        .store
        .complete_attempt(&a1.attempt_id, &complete_fail_req())
        .await
        .unwrap();
    let tv = state.store.show_task(&task.id).await.unwrap().unwrap();
    assert_eq!(tv.status, TaskStatus::Queued, "first failure must re-queue");

    // Attempt 2 fails -> budget exhausted -> task stays failed.
    let a2 = state.store.try_assign(&node_id).await.unwrap().unwrap();
    assert_ne!(a2.attempt_id, a1.attempt_id);
    state.store.ack_attempt(&a2.attempt_id).await.unwrap();
    state
        .store
        .complete_attempt(&a2.attempt_id, &complete_fail_req())
        .await
        .unwrap();
    let tv = state.store.show_task(&task.id).await.unwrap().unwrap();
    assert_eq!(tv.status, TaskStatus::Failed, "exhausted budget must fail");

    // A default task (max_attempts = 1) fails on the first attempt.
    let t1 = state
        .store
        .create_task(&agentgrid_common::CreateTaskRequest {
            prompt: "no-retry fixture".into(),
            repository: "demo".into(),
            adapter: "mock".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let b = state.store.try_assign(&node_id).await.unwrap().unwrap();
    state.store.ack_attempt(&b.attempt_id).await.unwrap();
    state
        .store
        .complete_attempt(&b.attempt_id, &complete_fail_req())
        .await
        .unwrap();
    let tv = state.store.show_task(&t1.id).await.unwrap().unwrap();
    assert_eq!(tv.status, TaskStatus::Failed, "default must not auto-retry");
    let _ = tok;
}
