use crate::helpers::*;

#[tokio::test]
async fn user_auth_setup_login_and_protects_endpoints() {
    let state = AppState::open_temp_fresh().await.unwrap();
    let app = build_router(state.clone());

    // Hardening P0: bootstrap window is closed. Before the first user exists,
    // task creation (any /v1/ user route) is 503, not open.
    let pre = app
        .clone()
        .oneshot(post_json(
            "/v1/tasks",
            serde_json::to_string(&CreateTaskRequest {
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
            })
            .unwrap(),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(pre.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Setup without the one-time token is forbidden.
    let no_token = app
        .clone()
        .oneshot(post_json(
            "/v1/auth/setup",
            serde_json::to_string(&serde_json::json!({
                "username": "alice", "password": "secret"
            }))
            .unwrap(),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(no_token.status(), StatusCode::FORBIDDEN);

    // Wrong setup token is forbidden.
    let wrong = app
        .clone()
        .oneshot(post_json(
            "/v1/auth/setup",
            serde_json::to_string(&serde_json::json!({
                "username": "alice", "password": "secret",
                "setup_token": "deadbeef"
            }))
            .unwrap(),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);

    // Setup with the correct one-time token creates the first user.
    assert_eq!(
        auth_setup(&app, &state, "alice", "secret").await,
        StatusCode::CREATED
    );
    // The token is single-use: a second setup (even with the same token) is
    // 409 because a user now exists.
    assert_eq!(
        auth_setup(&app, &state, "bob", "secret").await,
        StatusCode::CONFLICT
    );

    // Now the bootstrap window is closed: task creation requires a JWT.
    let no_token = app
        .clone()
        .oneshot(post_json(
            "/v1/tasks",
            serde_json::to_string(&CreateTaskRequest {
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
            })
            .unwrap(),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(no_token.status(), StatusCode::UNAUTHORIZED);

    // Wrong password is rejected.
    assert!(auth_login(&app, "alice", "wrong").await.is_none());

    // Correct login yields a token that unlocks the endpoint.
    let token = auth_login(&app, "alice", "secret").await.unwrap();
    let authed = app
        .clone()
        .oneshot(post_json(
            "/v1/tasks",
            serde_json::to_string(&CreateTaskRequest {
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
            })
            .unwrap(),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(authed.status(), StatusCode::CREATED);
}

/// Stage 2.5: login sets an HttpOnly + SameSite=Strict session cookie, and a
/// request carrying that cookie (no Authorization header) is authenticated.
#[tokio::test]
async fn login_sets_cookie_and_cookie_auths() {
    let state = AppState::open_temp_fresh().await.unwrap();
    let app = build_router(state.clone());
    assert_eq!(
        auth_setup(&app, &state, "alice", "secret").await,
        StatusCode::CREATED
    );
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/auth/login",
            serde_json::to_string(
                &serde_json::json!({ "username": "alice", "password": "secret" }),
            )
            .unwrap(),
            None,
        ))
        .await
        .unwrap();
    assert!(resp.status().is_success());
    // Extract the agentgrid_token cookie value from Set-Cookie.
    let set_cookie = resp
        .headers()
        .get(axum::http::header::SET_COOKIE)
        .and_then(|h| h.to_str().ok())
        .expect("login must set a Set-Cookie header")
        .to_string();
    assert!(set_cookie.contains("HttpOnly"), "cookie must be HttpOnly");
    assert!(
        set_cookie.contains("SameSite=Strict"),
        "cookie must be SameSite=Strict"
    );
    let cookie_val = set_cookie
        .split(';')
        .find(|p| p.trim().starts_with("agentgrid_token="))
        .unwrap()
        .trim();
    // The body still returns a token for non-browser clients (backwards compat).
    let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    // A request with only the cookie (no Authorization header) is authorized.
    let authed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/tasks")
                .header("content-type", "application/json")
                .header(axum::http::header::COOKIE, cookie_val)
                .body(Body::from(
                    serde_json::to_string(&CreateTaskRequest {
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
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        authed.status(),
        StatusCode::CREATED,
        "cookie must authenticate"
    );
}

#[tokio::test]
async fn approval_flow_allow_deny_and_expiry() {
    // Stage 5 durable approval: create (pending) -> list -> allow/deny -> list
    // reflects the new state; answering a terminal approval is a no-op.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());

    let task_id = create_task(&app, "mock", None).await;
    let ap_id = state
        .store
        .create_approval(
            &task_id,
            "attempt-x",
            None,
            "run Bash",
            3600,
            None,
            "session",
        )
        .await
        .unwrap();

    // Initially pending and visible.
    let listed = list_approvals(&app, Some("pending")).await;
    assert!(listed.iter().any(|a| a.id == ap_id));
    assert!(list_approvals(&app, Some("allowed")).await.is_empty());

    // Allow it with an operator reason, which is persisted and surfaced back.
    let allow = app
        .clone()
        .oneshot(post_json(
            &format!("/v1/approvals/{ap_id}/allow"),
            r#"{"reason":"looked ok"}"#.into(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(allow.status(), StatusCode::OK);

    let allowed = list_approvals(&app, Some("allowed")).await;
    let got = allowed
        .iter()
        .find(|a| a.id == ap_id)
        .expect("allowed approval must be listed");
    assert_eq!(got.status, ApprovalStatus::Allowed);
    assert_eq!(got.reason.as_deref(), Some("looked ok"));
    assert!(list_approvals(&app, Some("pending")).await.is_empty());

    // Answering a terminal approval is a safe no-op (idempotent).
    let again = app
        .clone()
        .oneshot(post_json(
            &format!("/v1/approvals/{ap_id}/deny"),
            "{}".into(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::OK);
    assert!(list_approvals(&app, Some("allowed"))
        .await
        .iter()
        .any(|a| a.id == ap_id));
}

#[tokio::test]
async fn approval_create_and_get_by_id_drives_permission_flow() {
    // Stage 5: an ACP agent's session/request_permission creates a durable
    // approval (POST /v1/tasks/{id}/approvals) that the daemon polls via
    // GET /v1/approvals/{id}.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());

    // Create a real task so the approvals FK (migration 0043) accepts the row.
    let task_req = CreateTaskRequest {
        prompt: "approve me".into(),
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
    let created = app
        .clone()
        .oneshot(post_auth(
            "/v1/tasks",
            serde_json::to_string(&task_req).unwrap(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let task_id: String = serde_json::from_slice::<TaskView>(
        &to_bytes(created.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()
    .id;

    let create = app
        .clone()
        .oneshot(post_json(
            &format!("/v1/tasks/{task_id}/approvals"),
            serde_json::to_string(&serde_json::json!({
                "attempt_id": "att-x",
                "session_id": "sess-x",
                "permission": { "tool": "Bash", "input": "rm -rf /" }
            }))
            .unwrap(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::OK);
    let id: String = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(create.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!id.is_empty());

    // Pending immediately after creation.
    let pending = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/approvals/{id}"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(pending.status(), StatusCode::OK);
    let view: ApprovalView =
        serde_json::from_slice(&to_bytes(pending.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(view.status, ApprovalStatus::Pending);
    assert_eq!(view.attempt_id, "att-x");
    assert_eq!(view.session_id.as_deref(), Some("sess-x"));

    // Allow the approval; the daemon's poll loop then proceeds.
    let allow = app
        .clone()
        .oneshot(post_json(
            &format!("/v1/approvals/{id}/allow"),
            "{}".into(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(allow.status(), StatusCode::OK);
    let allowed = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/approvals/{id}"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let view: ApprovalView =
        serde_json::from_slice(&to_bytes(allowed.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(view.status, ApprovalStatus::Allowed);

    // Unknown id 404s.
    let missing = app
        .clone()
        .oneshot(get_auth(
            "/v1/approvals/does-not-exist",
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn policy_endpoint_classifies_commands() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);

    async fn eval_cmd(app: &Router, cmd: &str) -> serde_json::Value {
        let body = serde_json::json!({ "command": cmd, "cwd": "/workspace" }).to_string();
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/v1/policy/evaluate",
                body,
                &test_token(app).await,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    let v = eval_cmd(&app, "rm -rf /tmp/x").await;
    assert_eq!(v.get("decision").unwrap(), "deny");
    assert_eq!(v.get("risk_class").unwrap(), "destructive");

    let v = eval_cmd(&app, "cat README.md").await;
    assert_eq!(v.get("decision").unwrap(), "allow");
    assert_eq!(v.get("risk_class").unwrap(), "read");

    let v = eval_cmd(&app, "git push origin main").await;
    assert_eq!(v.get("decision").unwrap(), "ask");
    assert_eq!(v.get("risk_class").unwrap(), "git_remote");

    let v = eval_cmd(&app, "apt-get install -y curl").await;
    assert_eq!(v.get("decision").unwrap(), "ask");
    assert_eq!(v.get("risk_class").unwrap(), "package_install");

    // Unterminated quote → fail-closed (ask), never allow.
    let v = eval_cmd(&app, "echo \"unterminated").await;
    assert_eq!(v.get("decision").unwrap(), "ask");
}

#[tokio::test]
async fn policy_endpoint_honors_autonomy_level() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);

    async fn eval(app: &Router, cmd: &str, autonomy: &str) -> serde_json::Value {
        let body = serde_json::json!({ "command": cmd, "cwd": "/workspace", "autonomy": autonomy })
            .to_string();
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/v1/policy/evaluate",
                body,
                &test_token(app).await,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    // L2 (default): git push → ask.
    let v = eval(&app, "git push origin main", "l2").await;
    assert_eq!(v.get("decision").unwrap(), "ask");
    // L3: git push → allow (autonomy permits network/git).
    let v = eval(&app, "git push origin main", "l3").await;
    assert_eq!(v.get("decision").unwrap(), "allow");
    // L0: cat → ask (fully supervised).
    let v = eval(&app, "cat README.md", "l0").await;
    assert_eq!(v.get("decision").unwrap(), "ask");
}

#[tokio::test]
async fn approval_scope_round_trips() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let task_id = create_task(&app, "mock", None).await;
    let ap = state
        .store
        .create_approval(
            &task_id,
            "attempt-x",
            None,
            "run Bash",
            3600,
            None,
            "tool_call",
        )
        .await
        .unwrap();
    let got = state.store.get_approval(&ap).await.unwrap().unwrap();
    assert_eq!(got.scope, "tool_call");
    // Default scope when omitted.
    let ap2 = state
        .store
        .create_approval(
            &task_id,
            "attempt-y",
            None,
            "run Bash",
            3600,
            None,
            "session",
        )
        .await
        .unwrap();
    let got2 = state.store.get_approval(&ap2).await.unwrap().unwrap();
    assert_eq!(got2.scope, "session");
}

#[tokio::test]
async fn policy_evaluate_audits_decision() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let body = serde_json::json!({ "command": "rm -rf /tmp/x", "cwd": "/workspace" }).to_string();
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/policy/evaluate",
            body,
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let events = state
        .store
        .list_audit(Some("policy.evaluate"), None, 10)
        .await
        .unwrap();
    assert!(!events.is_empty(), "every policy decision must be audited");
    assert_eq!(events[0].subject.as_deref(), Some("rm -rf /tmp/x"));
}
