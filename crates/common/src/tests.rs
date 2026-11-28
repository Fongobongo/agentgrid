use super::*;

#[test]
fn sha256_hex_matches_known_vector() {
    // FIPS 180-4 test vector; pins the output format across sha2 major
    // versions (audit X-D1: this hash round-trips CP↔node for drift
    // detection, so a format change must fail here, not in the field).
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

fn round_trip<T: Serialize + for<'de> Deserialize<'de>>(v: &T) -> T {
    let s = serde_json::to_string(v).unwrap();
    serde_json::from_str(&s).unwrap()
}

#[test]
fn status_enums_serde_snake_case() {
    assert_eq!(
        serde_json::to_string(&TaskStatus::Succeeded).unwrap(),
        "\"succeeded\""
    );
    let s: TaskStatus = serde_json::from_str("\"failed\"").unwrap();
    assert_eq!(s, TaskStatus::Failed);
    let a: AttemptStatus = serde_json::from_str("\"lost\"").unwrap();
    assert_eq!(a, AttemptStatus::Lost);
    let n: NodeStatus = serde_json::from_str("\"degraded\"").unwrap();
    assert_eq!(n, NodeStatus::Degraded);
}

#[test]
fn event_type_round_trip() {
    let e = EventType::Stdout;
    assert_eq!(round_trip(&e), e);
    let e = EventType::Result;
    assert_eq!(round_trip(&e), e);
}

#[test]
fn event_kind_round_trips_known_and_preserves_unknown() {
    for (kind, wire) in [
        (EventKind::Plan, "plan"),
        (EventKind::ToolCall, "tool_call"),
        (EventKind::ToolResult, "tool_result"),
        (EventKind::FileChange, "file_change"),
        (EventKind::PermissionRequest, "permission_request"),
        (EventKind::Usage, "usage"),
        (EventKind::Handoff, "handoff"),
        (EventKind::Cancel, "cancel"),
        (EventKind::Status, "status"),
        (EventKind::Log, "log"),
        (EventKind::Progress, "progress"),
        (EventKind::Result, "result"),
        (EventKind::Error, "error"),
    ] {
        assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{wire}\""));
        assert_eq!(round_trip(&kind), kind);
    }
    // Unknown kinds are preserved verbatim, never an error.
    let unknown: EventKind = serde_json::from_str("\"future_event\"").unwrap();
    assert_eq!(unknown, EventKind::Other("future_event".into()));
    assert_eq!(serde_json::to_string(&unknown).unwrap(), "\"future_event\"");
    assert_eq!(round_trip(&unknown), unknown);
}

#[test]
fn envelope_round_trip_and_maps_to_legacy_type() {
    let env = AgentEventEnvelope {
        version: 1,
        kind: EventKind::ToolCall,
        payload: serde_json::json!({ "name": "edit" }),
        raw_ref: None,
    };
    assert_eq!(round_trip(&env), env);
    assert_eq!(env.kind.to_event_type(), EventType::Tool);
    // Unknown kind inside an envelope still decodes and maps to a raw log.
    let unknown: AgentEventEnvelope =
        serde_json::from_str(r#"{"version":1,"kind":"weird","payload":{}}"#).unwrap();
    assert_eq!(unknown.kind, EventKind::Other("weird".into()));
    assert_eq!(unknown.kind.to_event_type(), EventType::Stdout);
}

#[test]
fn dto_round_trip() {
    let req = CreateTaskRequest {
        prompt: "write:hello.txt:hi".into(),
        repository: "demo".into(),
        adapter: "mock".into(),
        requested_node_id: Some("node-1".into()),
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
    };
    assert_eq!(round_trip(&req), req);
    // Competitor-gap feature (task-level auto-retry): absent on the wire
    // -> serde default 1 (no auto-retry).
    let parsed: CreateTaskRequest =
        serde_json::from_str(r#"{"prompt":"p","repository":"r","adapter":"mock"}"#).unwrap();
    assert_eq!(parsed.max_attempts, 1);

    // Competitor-gap feature (GitHub write-back): set fields round-trip;
    // None fields are omitted from the wire form (skip_serializing_if).
    let push_req = CreateTaskRequest {
        github_push: true,
        github_repo: Some("acme/demo".into()),
        github_issue: Some(42),
        github_base_ref: Some("main".into()),
        ..req.clone()
    };
    assert_eq!(round_trip(&push_req), push_req);
    let wire = serde_json::to_value(&req).unwrap();
    assert!(wire.get("github_push").is_none() || wire["github_push"] == false);
    assert!(wire.get("github_repo").is_none());
    let wire_push = serde_json::to_value(&push_req).unwrap();
    assert_eq!(wire_push["github_push"], true);
    assert_eq!(wire_push["github_repo"], "acme/demo");
    assert_eq!(wire_push["github_issue"], 42);

    let ev = TaskEvent {
        attempt_id: "a1".into(),
        sequence: 3,
        r#type: EventType::Stdout,
        payload: serde_json::json!({"text": "hi"}),
        created_at: "2026-01-01T00:00:00Z".into(),
        ingest_id: 42,
    };
    assert_eq!(round_trip(&ev), ev);

    // Pre-0037 serialized event (no ingest_id) still parses to 0.
    let old = r#"{"attempt_id":"a1","sequence":3,"type":"stdout","payload":{"text":"hi"},"created_at":"2026-01-01T00:00:00Z"}"#;
    let parsed: TaskEvent = serde_json::from_str(old).unwrap();
    assert_eq!(parsed.ingest_id, 0);

    let pr = PollResponse {
        assignment: Some(Assignment {
            attempt_id: "a1".into(),
            fencing_token: "f1".into(),
            task_id: "t1".into(),
            repository: "demo".into(),
            prompt: "x".into(),
            adapter: "mock".into(),
            number: 1,
            timeout_secs: 3600,
            git_url: String::new(),
            default_branch: String::new(),
            validation_command: None,
            validation_timeout_secs: None,
            base_commit: None,
            parent_acp_session_id: None,
            network_mode: None,
            provenance: None,
            upstream_commits: vec![],
            upstream_task_ids: vec![],
            group_id: None,
            read_only: false,
            eval_cases: vec![],
            consensus_group_id: None,
            consensus_member: None,
            opencode_override: None,
            github_push: false,
            github_repo: None,
            github_issue: None,
            github_base_ref: None,
            github_create: false,
            workflow_role: None,
            max_tokens: None,
            max_cost_cents: None,
        }),
        assignments: vec![],
        proxy_urls: vec![],
        proxy_urls_complete: true,
        adapter_env: vec![],
        adapter_env_complete: true,
    };
    assert_eq!(round_trip(&pr), pr);
    // Batch field round-trips and tolerates absence (N/N-1 compat).
    let legacy = r#"{"assignment":null}"#;
    let parsed: PollResponse = serde_json::from_str(legacy).unwrap();
    assert!(parsed.assignments.is_empty());
    // A pre-0.9 control plane omits the flag: empty must keep meaning
    // "nothing to say", not "you have no proxies".
    assert!(
        !parsed.proxy_urls_complete,
        "an absent flag must default to incomplete, not to a revocation"
    );
}

#[test]
fn enroll_dto_round_trip() {
    let er = EnrollRequest {
        token: "t".into(),
        name: "n".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        agent_version: "0.1".into(),
        protocol_version: None,
        permission_interception: "wrapper".into(),
        public_key: None,
    };
    assert_eq!(round_trip(&er), er);
    let hb = HeartbeatRequest {
        relay_url: None,
        relay_listen: None,
        adapter_env_delivery: None,
        status: Some(NodeStatus::Online),
        name: "n".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        agent_version: "0.1".into(),
        load_avg: 0.5,
        cpu_count: 4,
        free_memory_mb: 3840,
        free_disk_mb: 1024,
        active_attempts: 1,
        mem_available_mb: 4096,
        capabilities: vec![],
        protocol_version: None,
        public_key: None,
        discovered_skills: vec![],
        account_usage: vec![],
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
        applied_opencode_hash: None,
        active_rss_mib: 0,
        max_rss_mib: 0,
        capabilities_schema_version: None,
        supported_event_versions: None,
        egress_proxy: None,
        relay_hops: None,
    };
    assert_eq!(round_trip(&hb), hb);
    let resp = EnrollResponse {
        node_id: "node-1".into(),
        credential: "secret".into(),
    };
    assert_eq!(round_trip(&resp), resp);
}

#[test]
fn constant_time_eq_matches_only_equal_bytes() {
    assert!(constant_time_eq(b"abc", b"abc"));
    assert!(!constant_time_eq(b"abc", b"abd"));
    assert!(!constant_time_eq(b"abc", b"ab"));
}

#[test]
fn a_plain_task_uses_the_schema_default_for_attempts() {
    let req = CreateTaskRequest::new("do it", "demo", "mock");
    assert_eq!(req.max_attempts, 1);
    assert_eq!(req.prompt, "do it");
}
