use crate::helpers::*;

/// Plan 2.14 (#27): the capacity-pressure gate must respect the node's
/// reported RSS. Before the heartbeat `active_rss_mib` writer existed, the
/// gate always read 0 (migration added the column but nothing populated
/// it) and never rejected on real memory pressure — a node could be OOM-ing
/// and the scheduler kept dispatching. With the writer, a heartbeat reporting
/// RSS over `max_rss_mib` blocks the next `try_assign_batch` and records a
/// `metrics_capacity_pressure` row.
#[tokio::test]
async fn capacity_pressure_gate_uses_heartbeat_rss() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-cap", vec!["mock".into()], vec!["*".into()]).await;

    // Lower the node's hard memory ceiling so a realistic RSS sample trips
    // the gate (default max_rss_mib = 1024 MiB would need a ~1 GiB VmRSS).
    sqlx::query("UPDATE nodes SET max_rss_mib = ? WHERE id = ?")
        .bind(100i64)
        .bind(&node_id)
        .execute(&state.store.pool)
        .await
        .unwrap();

    // Heartbeat reports the node already at 90 MiB RSS. The gate forecast
    // for one new attempt is 256 MiB → 90 + 256 = 346 > 100 → reject.
    let hb = HeartbeatRequest {
        status: Some(NodeStatus::Online),
        name: "node-cap".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        agent_version: "t".into(),
        load_avg: 0.1,
        cpu_count: 4,
        free_memory_mb: 100_000,
        free_disk_mb: 100_000,
        mem_available_mb: 4096,
        active_attempts: 0,
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
        active_rss_mib: 90,
        max_rss_mib: 0,
        capabilities_schema_version: None,
        supported_event_versions: None,
        public_key: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&hb).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Persisted — confirms the writer landed the value on the nodes row.
    let stored: i64 = sqlx::query_scalar("SELECT active_rss_mib FROM nodes WHERE id = ?")
        .bind(&node_id)
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(
        stored, 90,
        "heartbeat must write active_rss_mib to the nodes row"
    );

    // Queue a task the node could serve.
    let task_id = create_task(&app, "mock", None).await;
    // The gate rejects: no assignment is returned.
    let got = state.store.try_assign_batch(&node_id, 4).await.unwrap();
    assert!(
        got.is_empty(),
        "gate must reject when active_rss_mib exceeds max_rss_mib"
    );
    // The queued task stayed pending.
    assert_eq!(show_status(&app, &task_id).await, TaskStatus::Queued);
    // And the rejection was recorded for observability.
    let rejects: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM metrics_capacity_pressure WHERE node_id = ?")
            .bind(&node_id)
            .fetch_one(&state.store.pool)
            .await
            .unwrap();
    assert_eq!(
        rejects, 1,
        "a capacity-pressure row must be recorded on rejection"
    );
}

/// Plan 2.14 write gap fix: `max_rss_mib` was pinned to the schema default
/// (1024 MiB) because the heartbeat never sent a value, so an operator on a
/// small host (Termux 256 / RPi 512) could never lower the gate and
/// OOM-pressure slipped through. Now the node can declare its own ceiling;
/// the heartbeat UPDATE only writes when the field is > 0, so a 0 (unset /
/// legacy build) leaves the row untouched.
/// GET /v1/nodes/{id} returns the CP view of one node (read by `ag node
/// doctor`) and 404s for unknown ids. The route had shipped DELETE-only.
#[tokio::test]
async fn get_node_returns_view_and_404() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, _cred) = enroll(&app, "node-get", vec!["mock".into()], vec!["*".into()]).await;
    let token = test_token(&app).await;

    let resp = app
        .clone()
        .oneshot(get_auth(&format!("/v1/nodes/{node_id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["id"].as_str(), Some(node_id.as_str()));
    assert_eq!(v["name"].as_str(), Some("node-get"));

    let resp = app
        .clone()
        .oneshot(get_auth(
            "/v1/nodes/00000000-0000-0000-0000-000000000000",
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn heartbeat_max_rss_mib_overrides_schema_default_only_when_set() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-ceiling", vec!["mock".into()], vec!["*".into()]).await;

    // Schema default after enrollment.
    let initial: i64 = sqlx::query_scalar("SELECT max_rss_mib FROM nodes WHERE id = ?")
        .bind(&node_id)
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(initial, 1024, "schema default should be 1024");

    // Heartbeat reporting max_rss_mib = 0 → CP must NOT overwrite the row
    // (CASE WHEN 0 > 0 path). The ceiling stays at the schema default.
    let hb_0 = HeartbeatRequest {
        status: Some(NodeStatus::Online),
        name: "node-ceiling".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        agent_version: "t".into(),
        load_avg: 0.1,
        cpu_count: 4,
        free_memory_mb: 100_000,
        free_disk_mb: 100_000,
        mem_available_mb: 4096,
        active_attempts: 0,
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
        public_key: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&hb_0).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let kept: i64 = sqlx::query_scalar("SELECT max_rss_mib FROM nodes WHERE id = ?")
        .bind(&node_id)
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(
        kept, 1024,
        "max_rss_mib=0 must not overwrite the existing ceiling (legacy/unset path)"
    );

    // Heartbeat reporting max_rss_mib = 256 (Termux/RPi class) → CP writes it.
    let mut hb_256 = hb_0.clone();
    hb_256.max_rss_mib = 256;
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&hb_256).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let lowered: i64 = sqlx::query_scalar("SELECT max_rss_mib FROM nodes WHERE id = ?")
        .bind(&node_id)
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(
        lowered, 256,
        "max_rss_mib>0 must write, so a small host lowers the gate"
    );
}

/// Audit follow-up (heartbeat offline TOCTOU): the heartbeat's status flip
/// and its `lose_node_attempts` sweep run in separate transactions. If a poll
/// re-onlines the node in that window and the scheduler hands it FRESH
/// assignments, an unguarded sweep fails those new attempts as `node_lost`.
/// The sweep (`lose_node_attempts_if_offline`) now re-checks inside its own
/// write transaction: a node whose row is no longer `offline` is skipped.
#[tokio::test]
async fn heartbeat_sweep_skips_node_reonlined_in_race_window() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-race-hb", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    assert_eq!(
        ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await,
        StatusCode::OK
    );

    // The race shape, step by step:
    // 1. Heartbeat: node reports offline -> row flips + in-flight attempt
    //    is lost (the normal path, must keep working).
    let hb = agentgrid_common::HeartbeatRequest {
        status: Some(agentgrid_common::NodeStatus::Offline),
        name: "node-race-hb".into(),
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
        public_key: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&hb).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let tv = show_task_view(&app, &assign.task_id).await;
    assert_eq!(tv.status, TaskStatus::Failed);
    assert_eq!(tv.error_code.as_deref(), Some("node_lost"));

    // 2. Race window: a poll re-onlines the node and the scheduler hands it
    //    a FRESH assignment before a stale sweep would run.
    sqlx::query("UPDATE nodes SET status = 'online' WHERE id = ?")
        .bind(&node_id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    let assign2 = create_and_assign(&app, &node_id, &cred, "write:hello2.txt:hi").await;
    assert_eq!(
        ack_attempt(&app, &assign2.attempt_id, &cred, &assign2.fencing_token).await,
        StatusCode::OK
    );

    // 3. A stale offline sweep fires now. With the guard it sees
    //    status='online' inside its txn and skips; the fresh attempt survives.
    state
        .store
        .lose_node_attempts_if_offline(&node_id)
        .await
        .unwrap();
    let tv = show_task_view(&app, &assign2.task_id).await;
    assert_eq!(
        tv.status,
        TaskStatus::Running,
        "fresh attempt survives the stale sweep"
    );

    // Control: when the node IS still offline, the same sweep loses it.
    sqlx::query("UPDATE nodes SET status = 'offline' WHERE id = ?")
        .bind(&node_id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    state
        .store
        .lose_node_attempts_if_offline(&node_id)
        .await
        .unwrap();
    let tv = show_task_view(&app, &assign2.task_id).await;
    assert_eq!(tv.status, TaskStatus::Failed);
    assert_eq!(tv.error_code.as_deref(), Some("node_lost"));
}

/// Stage 1.2: a node going offline with an in-flight attempt must lose it
/// (attempt=lost, task=failed/node_lost, capacity freed) and the task must be
/// retryable once the node is back online.
#[tokio::test]
async fn node_offline_loses_attempt_then_retry_succeeds() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-o", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;

    // Node reports offline -> its in-flight attempt is lost.
    let hb = HeartbeatRequest {
        status: Some(NodeStatus::Offline),
        name: "node-o".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        agent_version: "test".into(),
        load_avg: 0.0,
        cpu_count: 4,
        free_memory_mb: 100_000,
        free_disk_mb: 100_000,
        mem_available_mb: 4096,
        active_attempts: 1,
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
        public_key: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&hb).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let tv = show_task_view(&app, &assign.task_id).await;
    assert_eq!(tv.status, TaskStatus::Failed);
    assert_eq!(tv.error_code.as_deref(), Some("node_lost"));

    // Capacity freed: the node no longer accounts for the lost attempt.
    let nodes: serde_json::Value = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/nodes", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let na = nodes
        .get("items")
        .and_then(|v| v.as_array())
        .unwrap()
        .iter()
        .find(|n| n["id"] == node_id)
        .unwrap()["active_attempts"]
        .as_i64()
        .unwrap();
    assert_eq!(na, 0);

    // Node comes back online.
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&HeartbeatRequest {
                status: Some(NodeStatus::Online),
                ..hb
            })
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Retry -> re-queue -> re-assign to the recovered node -> succeed.
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

    let assign2 = loop {
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/v1/node/poll",
                serde_json::to_string(&PollRequest {
                    node_id: node_id.clone(),
                    name: "node-o".into(),
                    adapters: vec!["mock".into()],
                    repositories: vec!["*".into()],
                    max_concurrency: 2,
                    protocol_version: None,
                })
                .unwrap(),
                &cred,
            ))
            .await
            .unwrap();
        let pr: PollResponse =
            serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        if let Some(a) = pr.assignment {
            break a;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    // Acknowledge the new assignment before completing.
    ack_attempt(&app, &assign2.attempt_id, &cred, &assign2.fencing_token).await;
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/complete", assign2.attempt_id),
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
            &assign2.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Succeeded
    );
}

#[tokio::test]
async fn node_protocol_mismatch_marks_degraded() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let tk = app
        .clone()
        .oneshot(post_auth(
            "/v1/nodes/enrollment-token",
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(tk.status(), StatusCode::OK);
    let tkr: EnrollTokenResponse =
        serde_json::from_slice(&to_bytes(tk.into_body(), usize::MAX).await.unwrap()).unwrap();
    let req = EnrollRequest {
        token: tkr.token,
        name: "n1".into(),
        adapters: vec![],
        repositories: vec![],
        max_concurrency: 2,
        agent_version: "t".into(),
        protocol_version: Some("0".into()),
        permission_interception: "wrapper".into(),
        public_key: None,
    };
    let er = app
        .clone()
        .oneshot(post(
            "/v1/node/enroll",
            serde_json::to_string(&req).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(er.status(), StatusCode::OK);
    let er: EnrollResponse =
        serde_json::from_slice(&to_bytes(er.into_body(), usize::MAX).await.unwrap()).unwrap();
    let nodes = app
        .clone()
        .oneshot(get_auth("/v1/nodes", &test_token(&app).await))
        .await
        .unwrap();
    assert_eq!(nodes.status(), StatusCode::OK);
    let nodes: ListResponse<NodeView> =
        serde_json::from_slice(&to_bytes(nodes.into_body(), usize::MAX).await.unwrap()).unwrap();
    let node = nodes
        .items
        .iter()
        .find(|n| n.id == er.node_id)
        .expect("node present");
    assert_eq!(node.status, NodeStatus::Degraded);
}

#[tokio::test]
async fn heartbeat_auto_fills_skill_trust_ledger() {
    // Stage 9.2: a heartbeat that advertises discovered skills lands them in
    // the trust ledger as untrusted (so the operator can review them); a
    // later operator decision survives a subsequent discovery beat.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (_node_id, cred) = enroll(&app, "n-disc", vec!["mock".into()], vec!["*".into()]).await;

    let hb = HeartbeatRequest {
        status: Some(NodeStatus::Online),
        name: "n-disc".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        agent_version: "t".into(),
        load_avg: 0.0,
        cpu_count: 4,
        free_memory_mb: 100_000,
        free_disk_mb: 100_000,
        mem_available_mb: 4096,
        active_attempts: 0,
        capabilities: vec![],
        protocol_version: None,
        discovered_skills: vec![
            agentgrid_common::HeartbeatSkill {
                name: "git-helper".into(),
                source: "user".into(),
            },
            agentgrid_common::HeartbeatSkill {
                name: "ponytail".into(),
                source: "user".into(),
            },
        ],
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
        public_key: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&hb).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let list: Vec<SkillTrustView> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/skills", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let gh = list
        .iter()
        .find(|s| s.name == "git-helper" && s.source == "user")
        .expect("discovered skill landed in ledger");
    assert!(!gh.trusted, "discovery defaults untrusted");

    // Operator trusts it; a second discovery beat must not revert trust.
    let _ = app
        .clone()
        .oneshot(post_q_auth(
            "/v1/skills/git-helper/trust?source=user",
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&hb).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: SkillTrustView = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth(
                    "/v1/skills/git-helper?source=user",
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
    assert!(v.trusted, "operator decision survives re-discovery");
}

/// Hardening P0 item 5: a heartbeat advertising unsafe mode + interception is
/// persisted and surfaced on the node view.
#[tokio::test]
async fn heartbeat_persists_unsafe_active_and_interception() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-unsafe", vec!["mock".into()], vec!["*".into()]).await;
    let hb = HeartbeatRequest {
        status: Some(NodeStatus::Online),
        name: "node-unsafe".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        agent_version: "t".into(),
        load_avg: 0.0,
        cpu_count: 4,
        free_memory_mb: 100_000,
        free_disk_mb: 100_000,
        mem_available_mb: 4096,
        active_attempts: 0,
        capabilities: vec![],
        protocol_version: None,
        discovered_skills: vec![],
        unsafe_active: true,
        permission_interception: "wrapper".into(),
        // Hardening P2 item 35: report local storage pressure.
        outbox_bytes: 42,
        artifact_spool_bytes: 1337,
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
        public_key: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&hb).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(get_auth("/v1/nodes", &test_token(&app).await))
        .await
        .unwrap();
    let nodes: ListResponse<NodeView> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let mine = nodes
        .items
        .iter()
        .find(|n| n.id == node_id)
        .expect("node listed");
    assert!(mine.unsafe_active, "unsafe flag surfaced on node view");
    assert_eq!(mine.permission_interception, "wrapper");
    // Hardening P2 item 35: storage pressure surfaced on the node view.
    assert_eq!(mine.outbox_bytes, 42, "outbox bytes surfaced");
    assert_eq!(
        mine.artifact_spool_bytes, 1337,
        "artifact spool bytes surfaced"
    );
}

/// Plan 1.8 (#15): account usage reported via heartbeat surfaces at
/// `GET /v1/nodes/{id}/accounts/usage`.
#[tokio::test]
async fn node_account_usage_endpoint_returns_heartbeat_reported_usage() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-usage", vec!["mock".into()], vec!["*".into()]).await;

    let hb = HeartbeatRequest {
        status: Some(NodeStatus::Online),
        name: "node-usage".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        agent_version: "t".into(),
        load_avg: 0.0,
        cpu_count: 4,
        free_memory_mb: 100_000,
        free_disk_mb: 100_000,
        mem_available_mb: 4096,
        active_attempts: 0,
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
        account_usage: vec![agentgrid_common::AccountUsage {
            env: "ANTHROPIC_API_KEY".into(),
            token_index: 0,
            attempts: 7,
            rate_limited: 2,
        }],
        applied_opencode_hash: None,
        active_rss_mib: 0,
        max_rss_mib: 0,
        capabilities_schema_version: None,
        supported_event_versions: None,
        public_key: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&hb).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/nodes/{node_id}/accounts/usage"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let usage: Vec<agentgrid_common::AccountUsage> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].env, "ANTHROPIC_API_KEY");
    assert_eq!(usage[0].attempts, 7);
    assert_eq!(usage[0].rate_limited, 2);
}

/// Hardening P2 item 37: a drained node stops receiving NEW assignments
/// (its in-flight attempts keep running) and `--undrain` restores scheduling.
#[tokio::test]
async fn node_drain_blocks_new_assignments_until_undrained() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, _cred) = enroll(&app, "n-drain", vec!["mock".into()], vec!["*".into()]).await;

    // Helper: create a queued task through the API.
    async fn create_queued(app: &Router, prompt: &str) {
        let mk = serde_json::to_string(&CreateTaskRequest {
            prompt: prompt.into(),
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
            retry_on_node_loss: None,
            consensus_mode: None,
            review_of: None,
            max_tokens: None,
            max_cost_cents: None,
        })
        .unwrap();
        let r = app
            .clone()
            .oneshot(post_auth("/v1/tasks", mk, &test_token(app).await))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
    }

    // Use the store's try_assign directly instead of the long-poll route
    // (which holds the request open for the poll window when nothing is
    // assignable) — this makes the test fast and still exercises the same
    // scheduler path the drain flag gates.
    async fn try_assign_once(state: &AppState, node_id: &str) -> Option<Assignment> {
        state.store.try_assign(node_id).await.unwrap()
    }

    // Drain the node before any task exists.
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/nodes/{node_id}/drain?drain=true"),
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // A new task is NOT assigned to the drained node.
    create_queued(&app, "p-drained").await;
    let mut assigned = false;
    for _ in 0..10 {
        if try_assign_once(&state, &node_id).await.is_some() {
            assigned = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!assigned, "drained node must not receive a new assignment");

    // Undrain → the same task now assigns.
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/nodes/{node_id}/drain?drain=false"),
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut got = None;
    for _ in 0..20 {
        if let Some(a) = try_assign_once(&state, &node_id).await {
            got = Some(a);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        got.is_some(),
        "undrained node must receive the queued assignment"
    );
}

/// A lost attempt is the one failure the control plane has no evidence about:
/// the node died mid-run, so nothing is known about what the agent did. With
/// retry budget left, the task must go back to the queue and the retry must
/// land on a *different* node — that is the whole point of the feature, and
/// it is why `avoid_node_id` exists rather than just re-queueing.
#[tokio::test]
async fn node_loss_retries_the_task_on_a_different_node() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (a_id, a_cred) = enroll(&app, "node-a", vec!["mock".into()], vec!["*".into()]).await;
    let (b_id, b_cred) = enroll(&app, "node-b", vec!["mock".into()], vec!["*".into()]).await;

    // Budget of two attempts, default policy (node loss spends it).
    let task_id = create_task_with(&app, "write:hello.txt:hi", 2, None).await;
    let first = poll_assign(&app, &a_id, &a_cred)
        .await
        .expect("node-a takes the first attempt");
    assert_eq!(
        ack_attempt(&app, &first.attempt_id, &a_cred, &first.fencing_token).await,
        StatusCode::OK
    );

    // node-a dies with the attempt in flight.
    sqlx::query("UPDATE nodes SET status = 'offline' WHERE id = ?")
        .bind(&a_id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    state
        .store
        .lose_node_attempts_if_offline(&a_id)
        .await
        .unwrap();

    // Re-queued rather than failed, and the lost attempt is kept as the
    // audit trail (it is what tells an operator what may have happened).
    let tv = show_task_view(&app, &task_id).await;
    assert_eq!(
        tv.status,
        TaskStatus::Queued,
        "node loss with budget left must re-queue, not fail"
    );
    assert_eq!(tv.error_code, None);
    let lost: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM attempts WHERE task_id = ? AND status = 'lost'")
            .bind(&task_id)
            .fetch_one(&state.store.pool)
            .await
            .unwrap();
    assert_eq!(lost, 1, "the lost attempt stays for the audit trail");
    let audited: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE action = 'task.retry.auto' AND subject = ?",
    )
    .bind(&task_id)
    .fetch_one(&state.store.pool)
    .await
    .unwrap();
    assert_eq!(audited, 1, "the automatic retry is recorded");

    // Even back online, node-a must not get the retry.
    sqlx::query("UPDATE nodes SET status = 'online' WHERE id = ?")
        .bind(&a_id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    assert!(
        poll_assign(&app, &a_id, &a_cred).await.is_none(),
        "the node that lost the attempt must not receive the retry"
    );

    let retry = poll_assign(&app, &b_id, &b_cred)
        .await
        .expect("node-b takes the retry");
    assert_eq!(retry.task_id, task_id);
    assert_ne!(
        retry.attempt_id, first.attempt_id,
        "a new attempt, not a resume"
    );

    // The avoidance marker is consumed by the assignment it steered.
    let avoid: Option<String> = sqlx::query_scalar("SELECT avoid_node_id FROM tasks WHERE id = ?")
        .bind(&task_id)
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(
        avoid, None,
        "avoid_node_id is cleared once the retry is placed"
    );
}

/// The opt-out: tasks whose side effects must not be repeated keep the old
/// terminal behaviour, even with retry budget to spare.
#[tokio::test]
async fn node_loss_is_terminal_when_retry_on_node_loss_is_off() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-x", vec!["mock".into()], vec!["*".into()]).await;

    // Budget of three, but node loss is not allowed to spend it.
    let task_id = create_task_with(&app, "write:hello.txt:hi", 3, Some(false)).await;
    let first = poll_assign(&app, &node_id, &cred)
        .await
        .expect("node takes the attempt");
    assert_eq!(
        ack_attempt(&app, &first.attempt_id, &cred, &first.fencing_token).await,
        StatusCode::OK
    );

    sqlx::query("UPDATE nodes SET status = 'offline' WHERE id = ?")
        .bind(&node_id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    state
        .store
        .lose_node_attempts_if_offline(&node_id)
        .await
        .unwrap();

    let tv = show_task_view(&app, &task_id).await;
    assert_eq!(tv.status, TaskStatus::Failed);
    assert_eq!(tv.error_code.as_deref(), Some("node_lost"));
    let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attempts WHERE task_id = ?")
        .bind(&task_id)
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(attempts, 1, "the opt-out must not spawn a second attempt");
}

/// Avoidance is a preference, not a lock. On a single-node install there is
/// nowhere else to go, so the skip is lifted — retrying on the same node beats
/// leaving the task queued forever.
#[tokio::test]
async fn node_loss_retry_falls_back_to_the_only_node() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-only", vec!["mock".into()], vec!["*".into()]).await;

    let task_id = create_task_with(&app, "write:hello.txt:hi", 2, None).await;
    let first = poll_assign(&app, &node_id, &cred)
        .await
        .expect("the only node takes the attempt");
    assert_eq!(
        ack_attempt(&app, &first.attempt_id, &cred, &first.fencing_token).await,
        StatusCode::OK
    );

    sqlx::query("UPDATE nodes SET status = 'offline' WHERE id = ?")
        .bind(&node_id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    state
        .store
        .lose_node_attempts_if_offline(&node_id)
        .await
        .unwrap();
    assert_eq!(
        show_task_view(&app, &task_id).await.status,
        TaskStatus::Queued
    );

    // Node returns; it is the only candidate, so the skip must not deadlock.
    sqlx::query("UPDATE nodes SET status = 'online' WHERE id = ?")
        .bind(&node_id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    let retry = poll_assign(&app, &node_id, &cred)
        .await
        .expect("the only node gets the retry back");
    assert_eq!(retry.task_id, task_id);
    assert_ne!(retry.attempt_id, first.attempt_id);
}

/// Eligibility has to explain the skip, or a task parked by avoidance looks
/// like a scheduler bug from the outside.
#[tokio::test]
async fn eligibility_explains_the_avoided_node() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (a_id, a_cred) = enroll(&app, "node-ea", vec!["mock".into()], vec!["*".into()]).await;
    let (_b_id, _b_cred) = enroll(&app, "node-eb", vec!["mock".into()], vec!["*".into()]).await;

    let task_id = create_task_with(&app, "write:hello.txt:hi", 2, None).await;
    let first = poll_assign(&app, &a_id, &a_cred)
        .await
        .expect("node-ea takes the first attempt");
    assert_eq!(
        ack_attempt(&app, &first.attempt_id, &a_cred, &first.fencing_token).await,
        StatusCode::OK
    );
    sqlx::query("UPDATE nodes SET status = 'offline' WHERE id = ?")
        .bind(&a_id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    state
        .store
        .lose_node_attempts_if_offline(&a_id)
        .await
        .unwrap();
    sqlx::query("UPDATE nodes SET status = 'online' WHERE id = ?")
        .bind(&a_id)
        .execute(&state.store.pool)
        .await
        .unwrap();

    let el = state
        .store
        .task_eligibility(&task_id)
        .await
        .unwrap()
        .expect("task exists");
    let avoided = el
        .nodes
        .iter()
        .find(|n| n.node_id == a_id)
        .expect("the avoided node is listed");
    assert!(!avoided.eligible, "the avoided node is not a candidate");
    assert!(
        avoided
            .reasons
            .iter()
            .any(|r| r.contains("lost the previous attempt")),
        "the reason must say why: {:?}",
        avoided.reasons
    );
    assert!(
        el.nodes.iter().any(|n| n.node_id != a_id && n.eligible),
        "the other node remains eligible"
    );
}

/// A node that refuses an assignment (`ack ok=false`) never ran the work, so
/// there is nothing to be ambiguous about — with budget left the task must
/// retry, and on a different node. This path used to be terminal outright
/// because a requeue would have hot-looped on the refusing node; the
/// avoidance marker removes that reason.
#[tokio::test]
async fn rejected_assignment_retries_on_another_node() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (a_id, a_cred) = enroll(&app, "node-ra", vec!["mock".into()], vec!["*".into()]).await;
    let (b_id, b_cred) = enroll(&app, "node-rb", vec!["mock".into()], vec!["*".into()]).await;

    let task_id = create_task_with(&app, "write:hello.txt:hi", 2, None).await;
    let first = poll_assign(&app, &a_id, &a_cred)
        .await
        .expect("node-ra takes the first attempt");

    // The node refuses before acking: attempt lost, task back in the queue.
    assert!(state
        .store
        .reject_assignment(&first.attempt_id, "adapter_missing")
        .await
        .unwrap());

    let tv = show_task_view(&app, &task_id).await;
    assert_eq!(
        tv.status,
        TaskStatus::Queued,
        "a refusal with budget left must re-queue the task"
    );
    let audited: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE action = 'task.retry.auto' AND subject = ?",
    )
    .bind(&task_id)
    .fetch_one(&state.store.pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);

    // The refusing node does not get it back; the other one does.
    assert!(
        poll_assign(&app, &a_id, &a_cred).await.is_none(),
        "the refusing node must not be handed the retry"
    );
    let retry = poll_assign(&app, &b_id, &b_cred)
        .await
        .expect("node-rb takes the retry");
    assert_ne!(retry.attempt_id, first.attempt_id);
}

/// Without budget, a refusal stays what it was: the task fails.
#[tokio::test]
async fn rejected_assignment_is_terminal_without_budget() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-rt", vec!["mock".into()], vec!["*".into()]).await;

    let task_id = create_task_with(&app, "write:hello.txt:hi", 1, None).await;
    let first = poll_assign(&app, &node_id, &cred)
        .await
        .expect("node takes the attempt");
    assert!(state
        .store
        .reject_assignment(&first.attempt_id, "adapter_missing")
        .await
        .unwrap());

    assert_eq!(
        show_task_view(&app, &task_id).await.status,
        TaskStatus::Failed
    );
    let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attempts WHERE task_id = ?")
        .bind(&task_id)
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    assert_eq!(attempts, 1, "no budget means no second attempt");
}
