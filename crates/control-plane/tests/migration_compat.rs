//! Migration compatibility regression test (plan item: «Миграции schema без
//! изменения legacy happy path (E2E старого сценария зелёный до и после
//! миграции)»).
//!
//! Opens a fresh temp SQLite DB — which applies the full migration set
//! (`sqlx::migrate!("./migrations")`) — and walks the legacy happy path
//! end-to-end through the `Store` API: bootstrap user, enrollment token, node
//! enroll, heartbeat, task create, scheduler assign, event ingest, attempt
//! complete. The point is not to re-test each transition (the store unit
//! tests do that) but to assert the full migration set leaves the schema able
//! to serve the legacy happy path without a column/index drift breaking a
//! single step. If a new migration renames/drops a column the legacy path
//! uses, this test fails.

use agentgrid_common::{
    CompleteAttemptRequest, CreateTaskRequest, EnrollRequest, EventType, HeartbeatRequest,
    IncomingEvent, NodeStatus, TaskStatus,
};
use agentgrid_control_plane::store::Store;
use serde_json::json;

async fn temp_store() -> Store {
    // Disable critical disk watermark for tests (temp fs often has < 512 MB free).
    std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("ag-mig-{nanos}.db"));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(format!("{}-wal", p.display()));
    let _ = std::fs::remove_file(format!("{}-shm", p.display()));
    Store::open(p.to_str().unwrap()).await.unwrap()
}

#[tokio::test]
async fn migrations_serve_legacy_happy_path() {
    let s = temp_store().await;

    // 1. Bootstrap user (migration 0006).
    assert!(s
        .create_user("admin", "pw", agentgrid_common::ROLE_ADMIN)
        .await
        .unwrap());

    // 2. Enrollment token + node enroll (migrations 0003, 0001).
    let (token, _tok_id) = s.create_enrollment_token().await.unwrap();
    let enroll = EnrollRequest {
        token,
        name: "n1".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 1,
        agent_version: "test".into(),
        protocol_version: None,
        permission_interception: "wrapper".into(),
    };
    let resp = s.enroll_node(&enroll).await.unwrap().expect("enroll");
    let node_id = resp.node_id;

    // 3. Heartbeat → node online (migration 0001 + later alters).
    let hb = HeartbeatRequest {
        status: Some(NodeStatus::Online),
        name: "n1".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 1,
        agent_version: "test".into(),
        load_avg: 0.1,
        free_disk_mb: 4096,
        active_attempts: 0,
        protocol_version: None,
        capabilities: vec![],
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
        sandbox_backend: "none".into(),
        enforced_limits: false,
        repo_cache_bytes: 0,
        workspace_bytes: 0,
        network_mode: "none".into(),
        account_usage: vec![],
        applied_opencode_hash: None,
};
    assert!(s.heartbeat(&node_id, &hb).await.unwrap());

    // 4. Create a legacy task (no repo, no validation, plain-dir).
    let task = s
        .create_task(&CreateTaskRequest {
            prompt: "do thing".into(),
            repository: "*".into(),
            adapter: "mock".into(),
            requested_node_id: None,
            timeout_secs: Some(60),
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
        })
        .await
        .unwrap();
    assert_eq!(task.status, TaskStatus::Queued);

    // 5. Scheduler assigns the task to the online node (head-of-line path).
    let assign = s.try_assign(&node_id).await.unwrap().expect("assignment");
    assert_eq!(assign.task_id, task.id);
    assert_eq!(assign.adapter, "mock");

    // 6. Node ingests a couple of events (migration 0010 + later).
    let ack = s
        .ingest_events(
            &assign.attempt_id,
            &agentgrid_common::IngestEventsRequest {
                events: vec![
                    IncomingEvent {
                        sequence: 1,
                        r#type: EventType::Stdout,
                        payload: json!({"text":"line one"}),
                    },
                    IncomingEvent {
                        sequence: 2,
                        r#type: EventType::Stdout,
                        payload: json!({"text":"line two"}),
                    },
                ],
            },
        )
        .await
        .unwrap();
    assert_eq!(ack.accepted, 2);
    assert_eq!(ack.highest_contiguous_sequence, Some(2));

    // 7. Node completes the attempt → task succeeded (legacy outcome).
    s.complete_attempt(
        &assign.attempt_id,
        &CompleteAttemptRequest {
            exit_code: 0,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let final_task = s.show_task(&task.id).await.unwrap().expect("task present");
    assert_eq!(
        final_task.status,
        TaskStatus::Succeeded,
        "legacy happy path must reach succeeded after a clean completion"
    );

    // 8. Event continuity: both ingested events are retrievable in sequence
    // (proves the events table + sequence column survived migrations).
    let evs = s.get_events(&task.id, None, 0, None).await.unwrap();
    assert_eq!(evs.len(), 2, "both events must be retrievable");
    assert_eq!(evs[0].sequence, 1);
    assert_eq!(evs[1].sequence, 2);
    assert!(
        evs[0].ingest_id > 0,
        "ingest_id backfilled by migration 0037"
    );
    assert!(evs[1].ingest_id > evs[0].ingest_id);
}

/// Hardening P1 item 21: migration 0040 rebuilds attempts/task_events/artifacts
/// with FK constraints. A direct INSERT of an attempt whose node_id does not
/// exist must now be rejected by the database (backstop for the handler-level
/// ownership checks).
#[tokio::test]
async fn foreign_keys_enforced_after_migration_0040() {
    let s = temp_store().await;
    // Node + task first so the FK targets exist.
    let (token, _tok_id) = s.create_enrollment_token().await.unwrap();
    let enroll = EnrollRequest {
        token,
        name: "n".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        agent_version: "test".into(),
        protocol_version: None,
        permission_interception: "wrapper".into(),
    };
    let resp = s.enroll_node(&enroll).await.unwrap().expect("enroll");
    let node_id = resp.node_id;
    let task = s
        .create_task(&CreateTaskRequest {
            prompt: "x".into(),
            repository: "*".into(),
            adapter: "mock".into(),
            requested_node_id: None,
            timeout_secs: Some(60),
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
        })
        .await
        .unwrap();

    // Valid attempt (task_id + node_id exist) inserts fine.
    sqlx::query(
        "INSERT INTO attempts (id, task_id, number, node_id, status, started_at) \
         VALUES ('att-valid', ?, 1, ?, 'assigned', '2026-01-01T00:00:00Z')",
    )
    .bind(&task.id)
    .bind(&node_id)
    .execute(&s.pool)
    .await
    .unwrap();

    // Orphan attempt (task exists, node does NOT) must be rejected by FK.
    let res = sqlx::query(
        "INSERT INTO attempts (id, task_id, number, node_id, status, started_at) \
         VALUES ('att-orphan', ?, 2, 'no-such-node', 'assigned', '2026-01-01T00:00:00Z')",
    )
    .bind(&task.id)
    .execute(&s.pool)
    .await;
    assert!(
        res.is_err(),
        "attempt with a missing node_id must violate the FK"
    );

    // Orphan task event (attempt does not exist) must also be rejected.
    let res = sqlx::query(
        "INSERT INTO task_events (id, attempt_id, sequence, type, payload, created_at, ingest_id) \
         VALUES ('ev-orphan', 'no-such-attempt', 1, 'stdout', '{}', '2026-01-01T00:00:00Z', 1)",
    )
    .execute(&s.pool)
    .await;
    assert!(
        res.is_err(),
        "task event with a missing attempt_id must violate the FK"
    );
}
