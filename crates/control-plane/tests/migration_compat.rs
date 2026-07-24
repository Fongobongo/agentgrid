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
    assert!(s.create_user("admin", "pw").await.unwrap());

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
        })
        .await
        .unwrap();
    assert_eq!(task.status, TaskStatus::Queued);

    // 5. Scheduler assigns the task to the online node (head-of-line path).
    let assign = s
        .try_assign(&node_id)
        .await
        .unwrap()
        .expect("assignment");
    assert_eq!(assign.task_id, task.id);
    assert_eq!(assign.adapter, "mock");

    // 6. Node ingests a couple of events (migration 0010 + later).
    assert!(
        s.ingest_events(
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
        .unwrap()
    );

    // 7. Node completes the attempt → task succeeded (legacy outcome).
    s.complete_attempt(
        &assign.attempt_id,
        &CompleteAttemptRequest {
            exit_code: 0,
            commit_sha: None,
            error_code: None,
            acp_session_id: None,
            plan: None,
            provenance: None,
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
    let evs = s.get_events(&task.id, 0).await.unwrap();
    assert_eq!(evs.len(), 2, "both events must be retrievable");
    assert_eq!(evs[0].sequence, 1);
    assert_eq!(evs[1].sequence, 2);
}
