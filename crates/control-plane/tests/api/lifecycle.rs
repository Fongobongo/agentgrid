use crate::helpers::*;

#[tokio::test]
async fn full_task_lifecycle() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-1", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;

    // First event flips attempt+task to running.
    let ev = IngestEventsRequest {
        events: vec![IncomingEvent {
            sequence: 1,
            r#type: EventType::Stdout,
            payload: json!({"text": "start"}),
        }],
    };
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/events", assign.attempt_id),
            serde_json::to_string(&ev).unwrap(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

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
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Succeeded
    );
}

/// Plan 1.1 follow-up: a successful attempt must auto-create a pending
/// `task_patch_review` approval so the diff review UI can decide whether to
/// show the Accept/Reject/Rework buttons. Moves the whole flow end-to-end
/// (enroll → events → complete → GET `/v1/tasks/{id}/review-approval`) and
/// asserts the approval exists with `kind: patch_review`.
#[tokio::test]
async fn succeeded_attempt_creates_patch_review_approval() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-pr", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    let token = test_token(&app).await;

    // Flip attempt → running via a stdout event.
    let ev = IngestEventsRequest {
        events: vec![IncomingEvent {
            sequence: 1,
            r#type: EventType::Stdout,
            payload: json!({"text": "start"}),
        }],
    };
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/events", assign.attempt_id),
            serde_json::to_string(&ev).unwrap(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Complete as a success with a (mock) commit sha.
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: Some("deadbeefcafebabe".into()),
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
        TaskStatus::Succeeded
    );

    // Must have a pending patch-review approval attached to this task.
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{}/review-approval", assign.task_id),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let approval: Option<ApprovalView> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let approval = approval.expect("expected a pending patch-review approval");
    let perm: serde_json::Value =
        serde_json::from_str(&approval.permission).expect("permission json");
    assert_eq!(perm["kind"], "patch_review");
    assert_eq!(perm["task_id"], assign.task_id);
    assert_eq!(perm["attempt_id"], assign.attempt_id);
    assert_eq!(approval.status, ApprovalStatus::Pending);
    assert_eq!(approval.scope, "task_patch_review");
}

#[tokio::test]
async fn failure_marks_task_failed() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-2", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "fail:3").await; // Acknowledge the assignment before completing.
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
}

#[tokio::test]
async fn completion_propagates_provenance() {
    // Stage 13: a node tags a completion with an external-origin provenance
    // record; the CP persists it on the attempt row.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-prov", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    // Acknowledge the assignment before completing.
    ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await;
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
                plan: None,
                provenance: Some(agentgrid_common::ProvenanceRecord {
                    originator: "entire".into(),
                    external_id: "proj-42".into(),
                    label: Some("nightly".into()),
                    security_profile: None,
                }),
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
    // Read the stored record directly from the store.
    let stored: String = sqlx::query_scalar("SELECT provenance FROM attempts WHERE id = ?")
        .bind(&assign.attempt_id)
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    let p: agentgrid_common::ProvenanceRecord = serde_json::from_str(&stored).unwrap();
    assert_eq!(p.originator, "entire");
    assert_eq!(p.external_id, "proj-42");
    assert_eq!(p.label.as_deref(), Some("nightly"));
}

#[tokio::test]
async fn validation_failure_must_not_report_success() {
    // Stage 1.1 regression: a clean agent exit (exit_code 0) combined with a
    // validation failure must NOT be reported as success. The node reports the
    // distinct failure category via `error_code`; the control plane must decide
    // success by outcome, not the raw exit code.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-v", vec!["mock".into()], vec!["*".into()]).await;

    let req = CreateTaskRequest {
        prompt: "do thing".into(),
        repository: "demo".into(),
        adapter: "mock".into(),
        requested_node_id: None,
        timeout_secs: None,
        validation_command: Some("false".into()),
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
        retry_on_node_loss: None,
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
    let task_id: String =
        serde_json::from_slice::<TaskView>(&to_bytes(resp.into_body(), usize::MAX).await.unwrap())
            .unwrap()
            .id;

    // Long-poll for assignment, mirroring the node daemon.
    let poll_req = PollRequest {
        node_id: node_id.clone(),
        name: "n".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        protocol_version: None,
    };
    let mut assignment = None;
    for _ in 0..50 {
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/v1/node/poll",
                serde_json::to_string(&poll_req).unwrap(),
                &cred,
            ))
            .await
            .unwrap();
        let pr: PollResponse =
            serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        if let Some(a) = pr.assignment {
            assignment = Some(a);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let assignment = assignment.expect("task was never assigned");

    // Acknowledge the assignment before completing.
    ack_attempt(
        &app,
        &assignment.attempt_id,
        &cred,
        &assignment.fencing_token,
    )
    .await;

    // Agent exited 0 but validation failed -> node reports `validation_failed`.
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/complete", assignment.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: Some("validation_failed".into()),
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
            &assignment.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let tv: TaskView = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth(
                    &format!("/v1/tasks/{task_id}"),
                    &test_token(&app).await,
                ))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(tv.status, TaskStatus::Failed);
    assert_eq!(tv.error_code.as_deref(), Some("validation_failed"));
}

/// Plan 6.12 / 2.6: `/v1/version` is public (like the health probes) and
/// reports the CP crate version plus the node-facing contract versions so
/// `ag doctor` can surface daemon↔CP compatibility drift.
#[tokio::test]
async fn version_endpoint_reports_contracts() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    // No authorization header — must not 401.
    let resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/version")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        v.get("node_protocol").and_then(|x| x.as_str()),
        Some(agentgrid_common::NODE_PROTOCOL_VERSION)
    );
    assert_eq!(
        v.get("capabilities_schema").and_then(|x| x.as_str()),
        Some(agentgrid_common::CAPABILITIES_SCHEMA_VERSION)
    );
    assert_eq!(
        v.get("supported_event_versions").and_then(|x| x.as_str()),
        Some(agentgrid_common::SUPPORTED_EVENT_VERSIONS)
    );
    assert!(
        v.get("control_plane").and_then(|x| x.as_str()).is_some(),
        "crate version present: {v}"
    );
}
