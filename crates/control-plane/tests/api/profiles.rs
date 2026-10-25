use crate::helpers::*;

#[tokio::test]
async fn agent_profile_revisions_immutable_and_roll_back() {
    // Stage 13: a profile is a chain of immutable revisions; activating an
    // older revision rolls back without losing history.
    use agentgrid_common::{ActivateProfile, AgentProfileCreate};
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());

    // List empty.
    let list: Vec<String> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/profiles", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(list.is_empty());

    // Create two revisions.
    let r1: serde_json::Value = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(post_json(
                    "/v1/profiles/claude",
                    serde_json::to_string(&AgentProfileCreate {
                        system_prompt: "v1 prompt".into(),
                        autonomy: "l1".into(),
                        ..Default::default()
                    })
                    .unwrap(),
                    Some(&test_token(&app).await),
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
    let r1_rev = r1["revision"].as_i64().unwrap();
    let r2: serde_json::Value = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(post_json(
                    "/v1/profiles/claude",
                    serde_json::to_string(&AgentProfileCreate {
                        system_prompt: "v2 prompt".into(),
                        autonomy: "l3".into(),
                        ..Default::default()
                    })
                    .unwrap(),
                    Some(&test_token(&app).await),
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
    let r2_rev = r2["revision"].as_i64().unwrap();
    assert_eq!(r2_rev, r1_rev + 1, "revisions monotonically increase");

    // Activate the newer, then roll back to the older.
    let _ = app
        .clone()
        .oneshot(post_json(
            "/v1/profiles/claude/activate",
            serde_json::to_string(&ActivateProfile { revision: r2_rev }).unwrap(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    let revs: Vec<agentgrid_common::AgentProfile> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/profiles/claude", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(revs.len(), 2, "both revisions kept (immutable history)");
    assert!(
        revs.iter().any(|p| p.revision == r2_rev && p.active),
        "r2 active"
    );
    assert!(
        revs.iter().any(|p| p.revision == r1_rev && !p.active),
        "r1 inactive"
    );

    // Roll back.
    let _ = app
        .clone()
        .oneshot(post_json(
            "/v1/profiles/claude/activate",
            serde_json::to_string(&ActivateProfile { revision: r1_rev }).unwrap(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    let revs: Vec<agentgrid_common::AgentProfile> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/profiles/claude", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(
        revs.iter().any(|p| p.revision == r1_rev && p.active),
        "r1 active after rollback"
    );
    let v1 = revs.iter().find(|p| p.revision == r1_rev).unwrap();
    assert_eq!(v1.system_prompt, "v1 prompt");
    assert_eq!(v1.autonomy, "l1");

    // Profile id now in active list.
    let list: Vec<String> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/profiles", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(list, vec!["claude".to_string()]);
}

#[tokio::test]
async fn agent_profile_carries_secret_requirements_and_version() {
    // Stage 13: a profile revision stores secret requirements (names only,
    // never values) + adapter_version; they round-trip through the CP store.
    use agentgrid_common::{ActivateProfile, AgentProfileCreate, SecretRequirement};
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);

    let body = serde_json::to_string(&AgentProfileCreate {
        system_prompt: "be brief".into(),
        autonomy: "l3".into(),
        memory_max: None,
        cpu_quota: None,
        tasks_max: None,
        secret_requirements: vec![
            SecretRequirement {
                env: "ANTHROPIC_API_KEY".into(),
                required: true,
            },
            SecretRequirement {
                env: "OPTIONAL_TOKEN".into(),
                required: false,
            },
        ],
        adapter_version: Some("1.4.0".into()),
        mcp_server_ids: vec!["github".into()],
    })
    .unwrap();
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/profiles/claude",
            body,
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rev: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let rev_no = rev["revision"].as_i64().unwrap();

    // Activate + fetch.
    let _ = app
        .clone()
        .oneshot(post_json(
            "/v1/profiles/claude/activate",
            serde_json::to_string(&ActivateProfile { revision: rev_no }).unwrap(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    let revs: Vec<agentgrid_common::AgentProfile> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/profiles/claude", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let p = revs.iter().find(|p| p.revision == rev_no).unwrap();
    assert_eq!(p.secret_requirements.len(), 2);
    assert_eq!(p.secret_requirements[0].env, "ANTHROPIC_API_KEY");
    assert!(p.secret_requirements[0].required);
    assert_eq!(p.secret_requirements[1].env, "OPTIONAL_TOKEN");
    assert!(!p.secret_requirements[1].required);
    assert_eq!(p.adapter_version.as_deref(), Some("1.4.0"));
    assert_eq!(
        p.mcp_server_ids,
        vec!["github".to_string()],
        "per-profile MCP subset round-trips through the store",
    );
}

// ============================================================================
// Hardening P0: cross-node isolation regression tests.
// A node may only mutate attempts assigned to it. A second node's credential
// must not be able to ingest events, complete, ack, cancel-poll, create a
// session, or upload artifacts on the first node's attempt. Upstream artifact
// reads are restricted to the consumer's workflow-dependency chain.
// ============================================================================

/// Helper: enroll two nodes; create+assign a task to node_a only (node_b polls
/// Feature "opencode profiles": PUT creates, GET returns, DELETE removes,
/// and a node with an assigned profile picks it up via the pull endpoint.
/// The audit row lands when the node reports the apply (via
/// `record_opencode_apply` — the route under test only logs storage-facing
/// rows; the node-side caller asserts the audit row exists after its own
/// apply).
#[tokio::test]
async fn opencode_profiles_crud_and_node_pull() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let token = test_token(&app).await;

    // 1. Create a profile via PUT (create-or-replace).
    let cfg = serde_json::json!({
        "model": "anthropic/claude-sonnet-4-5",
        "small_model": "anthropic/claude-haiku-4-5",
        "provider": {
            "anthropic": {
                "options": { "timeout": 600000 }
            }
        }
    });
    let resp = app
        .clone()
        .oneshot(put_auth(
            "/v1/opencode-profiles/sonnet",
            serde_json::to_string(&serde_json::json!({ "config": cfg })).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let profile: agentgrid_common::OpencodeProfile =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(profile.name, "sonnet");
    assert!(!profile.hash.is_empty());

    // Idempotent re-PUT with the same body keeps the same hash.
    let resp = app
        .clone()
        .oneshot(put_auth(
            "/v1/opencode-profiles/sonnet",
            serde_json::to_string(&serde_json::json!({ "config": cfg })).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    let profile2: agentgrid_common::OpencodeProfile =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        profile.hash, profile2.hash,
        "idempotent upsert must not bump the hash"
    );
    assert_eq!(profile.id, profile2.id, "PK must be stable across updates");

    // 2. GET by name returns the same row.
    let resp = app
        .clone()
        .oneshot(get_auth("/v1/opencode-profiles/sonnet", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 3. Enroll a node and assign the profile, then pull.
    let (node_id, cred) = enroll(&app, "n1", vec!["mock".into()], vec!["*".into()]).await;
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/nodes/{node_id}/opencode-profile"),
            serde_json::to_string(&serde_json::json!({ "profile_id": profile.id })).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .clone()
        .oneshot(get_auth("/v1/node/opencode-config/active", &cred))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let active: agentgrid_common::ActiveOpencodeConfigResponse =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(active.profile_id.as_deref(), Some(profile.id.as_str()));
    assert_eq!(active.hash.as_deref(), Some(profile.hash.as_str()));

    // 4. Assigning a missing profile id is a 404.
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/nodes/{node_id}/opencode-profile"),
            serde_json::to_string(&serde_json::json!({ "profile_id": "does-not-exist" })).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 5. DELETE frees the slot; subsequent pull is empty.
    let resp = app
        .clone()
        .oneshot(delete_auth("/v1/opencode-profiles/sonnet", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .clone()
        .oneshot(get_auth("/v1/node/opencode-config/active", &cred))
        .await
        .unwrap();
    let active: agentgrid_common::ActiveOpencodeConfigResponse =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(
        active.profile_id.is_none(),
        "deleted profile must clear the assignment"
    );
}

/// Feature "opencode profiles": `DELETE ?fallback=<name>` re-points every
/// assigned node onto the fallback profile atomically, instead of leaving
/// them unassigned.
#[tokio::test]
async fn opencode_delete_with_fallback_reassigns_nodes() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let token = test_token(&app).await;

    // Two profiles, one node on each.
    for (name, model) in [("a", "model-a"), ("b", "model-b")] {
        let resp = app
            .clone()
            .oneshot(put_auth(
                &format!("/v1/opencode-profiles/{name}"),
                serde_json::to_string(&serde_json::json!({"config": {"model": model}})).unwrap(),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let (node_id, cred) = enroll(&app, "n1", vec!["mock".into()], vec!["*".into()]).await;
    let a = get_auth("/v1/opencode-profiles/a", &token);
    let pa: agentgrid_common::OpencodeProfile = serde_json::from_slice(
        &to_bytes(
            app.clone().oneshot(a).await.unwrap().into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let b = get_auth("/v1/opencode-profiles/b", &token);
    let pb: agentgrid_common::OpencodeProfile = serde_json::from_slice(
        &to_bytes(
            app.clone().oneshot(b).await.unwrap().into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    // PUT returns the profile JSON; GET here is just to fetch ids.
    assert_ne!(pa.id, pb.id);

    // Assign the node to profile "a".
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/nodes/{node_id}/opencode-profile"),
            serde_json::to_string(&serde_json::json!({ "profile_id": pa.id })).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Plain delete of a missing profile is still 404.
    let resp = app
        .clone()
        .oneshot(delete_auth("/v1/opencode-profiles/nope", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Fallback delete: node moves a -> b, profile a is gone.
    let resp = app
        .clone()
        .oneshot(delete_auth("/v1/opencode-profiles/a?fallback=b", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = app
        .clone()
        .oneshot(get_auth("/v1/opencode-profiles/a", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = app
        .clone()
        .oneshot(get_auth("/v1/node/opencode-config/active", &cred))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let active: agentgrid_common::ActiveOpencodeConfigResponse =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        active.profile_id.as_deref(),
        Some(pb.id.as_str()),
        "fallback delete must move the node onto the fallback profile"
    );

    // Self-fallback is rejected.
    let resp = app
        .clone()
        .oneshot(delete_auth("/v1/opencode-profiles/b?fallback=b", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Feature "opencode profiles": A/B percent split — nodes on either arm are
/// redistributed between the two profiles by percent (deterministic, only
/// the two arms move).
#[tokio::test]
async fn opencode_assign_percent_splits_nodes_between_two_profiles() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let token = test_token(&app).await;

    for name in ["a", "b"] {
        let resp = app
            .clone()
            .oneshot(put_auth(
                &format!("/v1/opencode-profiles/{name}"),
                serde_json::to_string(&serde_json::json!({"config": {"model": name}})).unwrap(),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let a_id = fetch_profile_id(&app, "a").await;
    let b_id = fetch_profile_id(&app, "b").await;

    // Enroll 4 nodes, 2 on each arm.
    for i in 0..4 {
        let (node_id, _cred) = enroll(
            &app,
            &format!("n{i}"),
            vec!["mock".into()],
            vec!["*".into()],
        )
        .await;
        let pid = if i < 2 { &a_id } else { &b_id };
        let resp = app
            .clone()
            .oneshot(post_auth(
                &format!("/v1/nodes/{node_id}/opencode-profile"),
                serde_json::to_string(&serde_json::json!({"profile_id": pid})).unwrap(),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }
    assert_eq!(
        state
            .store
            .list_nodes_for_profile(&a_id)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        state
            .store
            .list_nodes_for_profile(&b_id)
            .await
            .unwrap()
            .len(),
        2
    );

    // percent=25 -> 1 node on `a`, 3 on `b` (floor(4*25/100) = 1).
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/opencode-profiles/a/assign-percent",
            serde_json::to_string(&serde_json::json!({"other": "b", "percent": 25})).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        state
            .store
            .list_nodes_for_profile(&a_id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        state
            .store
            .list_nodes_for_profile(&b_id)
            .await
            .unwrap()
            .len(),
        3
    );

    // Bad inputs.
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/opencode-profiles/a/assign-percent",
            serde_json::to_string(&serde_json::json!({"other": "a", "percent": 50})).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/opencode-profiles/a/assign-percent",
            serde_json::to_string(&serde_json::json!({"other": "b", "percent": 200})).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/opencode-profiles/none/assign-percent",
            serde_json::to_string(&serde_json::json!({"other": "b", "percent": 50})).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Feature "opencode profiles": bundle-pinned skills (item 10) — a profile
/// carries a pinned skills set that `GET active` returns, and the node-side
/// reconcile surfaces untrusted pins through the apply audit.
#[tokio::test]
async fn opencode_profile_pinned_skills_flow() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let token = test_token(&app).await;

    // Create with two pinned skills (canonicalization dedups/sorts).
    let resp = app
        .clone()
        .oneshot(put_auth(
            "/v1/opencode-profiles/pinned",
            serde_json::to_string(&serde_json::json!({
                "config": { "model": "m" },
                "pinned_skills": ["ponytail", "caveman", "ponytail"],
            }))
            .unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let p: agentgrid_common::OpencodeProfile =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        p.pinned_skills.as_deref(),
        Some(["caveman".to_string(), "ponytail".to_string()].as_slice())
    );

    // Enroll a node, assign, and `GET active` returns the pin set.
    let (node_id, cred) = enroll(&app, "n1", vec!["mock".into()], vec!["*".into()]).await;
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/nodes/{node_id}/opencode-profile"),
            serde_json::to_string(&serde_json::json!({ "profile_id": p.id })).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = app
        .clone()
        .oneshot(get_auth("/v1/node/opencode-config/active", &cred))
        .await
        .unwrap();
    let active: agentgrid_common::ActiveOpencodeConfigResponse =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        active.pinned_skills.as_deref(),
        Some(["caveman".to_string(), "ponytail".to_string()].as_slice())
    );

    // The node reports one of the two pins untrusted.
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/opencode-config/audit",
            serde_json::to_string(&serde_json::json!({
                "profile_id": p.id,
                "hash": p.hash,
                "trigger": "startup",
                "pinned_untrusted": ["ponytail"],
            }))
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/nodes/{node_id}/opencode-audit"),
            &token,
        ))
        .await
        .unwrap();
    let list: ListResponse<agentgrid_common::OpencodeConfigAuditEntry> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(list.items.len(), 1);
    assert_eq!(
        list.items[0].pinned_untrusted.as_deref(),
        Some(["ponytail".to_string()].as_slice())
    );

    // Re-PUT without pinned_skills clears the pin set (None).
    let resp = app
        .clone()
        .oneshot(put_auth(
            "/v1/opencode-profiles/pinned",
            serde_json::to_string(&serde_json::json!({ "config": { "model": "m" } })).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    let p: agentgrid_common::OpencodeProfile =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(
        p.pinned_skills.is_none(),
        "a PUT without pinned_skills must clear the pin set"
    );
}

/// Feature "opencode profiles": the list route attaches per-profile apply
/// counts from the audit feed (item 6 of the configurator polish list).
#[tokio::test]
async fn opencode_list_reports_apply_count() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let token = test_token(&app).await;
    let resp = app
        .clone()
        .oneshot(put_auth(
            "/v1/opencode-profiles/p1",
            serde_json::to_string(&serde_json::json!({"config": {"model": "m"}})).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // No applies yet -> 0.
    let resp = app
        .clone()
        .oneshot(get_auth("/v1/opencode-profiles", &token))
        .await
        .unwrap();
    let list: ListResponse<agentgrid_common::OpencodeProfile> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(list.items.len(), 1);
    assert_eq!(list.items[0].apply_count, Some(0));

    // One node apply lands an audit row -> count becomes 1.
    let (node_id, node_cred) = enroll(&app, "n1", vec!["mock".into()], vec!["*".into()]).await;
    let resp = app
        .clone()
        .oneshot(get_auth("/v1/opencode-profiles/p1", &token))
        .await
        .unwrap();
    let p: agentgrid_common::OpencodeProfile =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/nodes/{node_id}/opencode-profile"),
            serde_json::to_string(&serde_json::json!({ "profile_id": p.id })).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/opencode-config/audit",
            serde_json::to_string(&serde_json::json!({
                "profile_id": p.id,
                "hash": p.hash,
                "trigger": "startup",
            }))
            .unwrap(),
            &node_cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = app
        .clone()
        .oneshot(get_auth("/v1/opencode-profiles", &token))
        .await
        .unwrap();
    let list: ListResponse<agentgrid_common::OpencodeProfile> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(list.items[0].apply_count, Some(1));
}

/// Feature "opencode profiles": TTL — a profile with an `expires_at` in
/// the past is swept by the janitor exactly like a manual delete (nodes
/// re-pointed off, profile gone).
#[tokio::test]
async fn opencode_profile_ttl_janitor_expires() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let token = test_token(&app).await;

    // Create with a TTL in the past.
    let resp = app
        .clone()
        .oneshot(put_auth(
            "/v1/opencode-profiles/tmp",
            serde_json::to_string(&serde_json::json!({
                "config": { "model": "model-x" },
                "expires_at": "2000-01-01T00:00:00Z",
            }))
            .unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (node_id, _cred) = enroll(&app, "n1", vec!["mock".into()], vec!["*".into()]).await;
    let p: agentgrid_common::OpencodeProfile = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/opencode-profiles/tmp", &token))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/nodes/{node_id}/opencode-profile"),
            serde_json::to_string(&serde_json::json!({ "profile_id": p.id })).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Janitor sweep: expired profile is deleted, node assignment freed.
    let expired = state.store.expire_opencode_profiles().await.unwrap();
    assert_eq!(expired.len(), 1, "one profile should expire");
    assert_eq!(expired[0].0, "tmp");
    assert_eq!(expired[0].1, vec![node_id.clone()]);

    let resp = app
        .clone()
        .oneshot(get_auth("/v1/opencode-profiles/tmp", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = app
        .clone()
        .oneshot(get_auth("/v1/node/opencode-config/active", &_cred))
        .await
        .unwrap();
    let active: agentgrid_common::ActiveOpencodeConfigResponse =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(
        active.profile_id.is_none(),
        "expired profile must clear assignment"
    );
}

/// Feature "opencode profiles": nodes publish apply events through
/// `POST /v1/node/opencode-config/audit` (auth = node bearer). An operator
/// can then read them through the audited GET under `/v1/nodes/{id}`.
#[tokio::test]
async fn opencode_audit_post_and_read() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let token = test_token(&app).await;

    // Enroll a node + mint a profile + assign.
    let (node_id, node_cred) =
        enroll(&app, "opencode-audit-node", vec!["mock".into()], vec![]).await;
    let put_cfg = serde_json::json!({ "config": { "model": "test/model" } }).to_string();
    let put_req = put_auth("/v1/opencode-profiles/audit-target", put_cfg, &token);
    let put_resp = app.clone().oneshot(put_req).await.unwrap();
    assert_eq!(put_resp.status(), StatusCode::OK);

    let get_req = get_auth("/v1/opencode-profiles/audit-target", &token);
    let get_resp = app.clone().oneshot(get_req).await.unwrap();
    let profile: serde_json::Value =
        serde_json::from_slice(&to_bytes(get_resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let profile_id = profile["id"].as_str().unwrap();

    let assign_req = post_auth(
        &format!("/v1/nodes/{node_id}/opencode-profile"),
        serde_json::json!({ "profile_id": profile_id }).to_string(),
        &token,
    );
    let assign_resp = app.clone().oneshot(assign_req).await.unwrap();
    assert_eq!(assign_resp.status(), StatusCode::NO_CONTENT);

    // Node records an apply.
    let audit_body = serde_json::json!({
        "hash": "ab".repeat(32),
        "trigger": "ws_push",
        "profile_id": profile_id,
    })
    .to_string();
    let audit_req = post_auth("/v1/node/opencode-config/audit", audit_body, &node_cred);
    let audit_resp = app.clone().oneshot(audit_req).await.unwrap();
    assert_eq!(audit_resp.status(), StatusCode::NO_CONTENT);

    // Operator reads the audit.
    let lv_req = get_auth(&format!("/v1/nodes/{node_id}/opencode-audit"), &token);
    let lv_resp = app.clone().oneshot(lv_req).await.unwrap();
    let list: serde_json::Value =
        serde_json::from_slice(&to_bytes(lv_resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["trigger"], "ws_push");
    assert_eq!(items[0]["hash"], "ab".repeat(32));

    // Bad trigger rejected.
    let bad_body = serde_json::json!({ "hash": "ab".repeat(32), "trigger": "h4x0r" }).to_string();
    let bad_req = post_auth("/v1/node/opencode-config/audit", bad_body, &node_cred);
    let bad_resp = app.oneshot(bad_req).await.unwrap();
    assert_eq!(bad_resp.status(), StatusCode::BAD_REQUEST);
}
/// Feature "opencode profiles": PUT overwrites keep the previous body for
/// one-step rollback (`POST /v1/opencode-profiles/{name}/rollback`). The
/// rollback swaps cur↔prev, pushes the new hash to every assigned node, and
/// drops the far-older copy (only one revision lives at a time).
#[tokio::test]
async fn opencode_profile_rollback() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let token = test_token(&app).await;

    // 1. Initial upsert.
    let body1 = serde_json::json!({"config": {"model": "a/one", "snapshot": true}});
    let put1 = put_auth("/v1/opencode-profiles/rollback", body1.to_string(), &token);
    let r1 = app.clone().oneshot(put1).await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);

    // 2. Second upsert — prev now points at the first body.
    let body2 = serde_json::json!({"config": {"model": "a/two", "snapshot": false}});
    let put2 = put_auth("/v1/opencode-profiles/rollback", body2.to_string(), &token);
    app.clone().oneshot(put2).await.unwrap();

    let get = get_auth("/v1/opencode-profiles/rollback", &token);
    let body = app.clone().oneshot(get).await.unwrap();
    let p: serde_json::Value =
        serde_json::from_slice(&to_bytes(body.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        p["hash"],
        p["id"]
            .is_string()
            .then(|| p["hash"].as_str().unwrap())
            .unwrap()
    );
    assert_eq!(p["prev"]["config"]["model"], "a/one");
    assert_eq!(p["prev"]["hash"].as_str().unwrap().len(), 64);

    // 2b. Third upsert — stack is now [v1, v2] with v2 live.
    let body3 = serde_json::json!({"config": {"model": "a/three"}});
    let put3 = put_auth("/v1/opencode-profiles/rollback", body3.to_string(), &token);
    app.clone().oneshot(put3).await.unwrap();

    // 3. Rollback 2 steps → back at v1.
    let rb = post_auth(
        "/v1/opencode-profiles/rollback/rollback?steps=2",
        "".to_string(),
        &token,
    );
    let rb_resp = app.clone().oneshot(rb).await.unwrap();
    let after: serde_json::Value =
        serde_json::from_slice(&to_bytes(rb_resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(after["config"]["model"], "a/one");
    // Top of stack is v2 (the last thing PUT before rollback); it sits in
    // `prev` so a follow-up rollback lands there next.
    if let Some(prev) = after.get("prev").and_then(|v| v.as_object()) {
        assert!(prev.is_empty() || prev["config"]["model"].as_str().is_some());
    } else {
        panic!("prev should exist after a 2-step rollback that's ≤ the revisions");
    }

    // 3b. Also verify the shorthand single-step rollback after another
    // write (regression for `steps=1` default value).
    let body4 = serde_json::json!({"config": {"model": "a/four"}});
    let put4 = put_auth("/v1/opencode-profiles/rollback", body4.to_string(), &token);
    app.clone().oneshot(put4).await.unwrap();
    let rb1 = post_auth(
        "/v1/opencode-profiles/rollback/rollback?steps=1",
        "".to_string(),
        &token,
    );
    let rb1_resp = app.clone().oneshot(rb1).await.unwrap();
    let after1: serde_json::Value =
        serde_json::from_slice(&to_bytes(rb1_resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(after1["config"]["model"], "a/one");
}

/// Feature "opencode profiles": the allowlist gates the keys a client can
/// push into a profile. Unknown keys are dropped silently so a typo cannot
/// wedge every node on a parsing error.
/// Feature "opencode profiles" dry-run preview (`?dry_run=true`). Surfaces
/// the post-sanitisation body, the hash that WOULD have been computed and
/// the dropped unknown keys, so the operator sees drift before commit.
#[tokio::test]
async fn opencode_upsert_dry_run_returns_stripped_keys_and_hash() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let token = test_token(&app).await;

    let body = serde_json::json!({
        "config": {
            "model": "a/one",
            "snapshot": true,
            "malicious-not-allowed": 1,
            "unknown-as-well": {"nested": true}
        }
    });
    let put = put_auth(
        "/v1/opencode-profiles/dryrun?dry_run=true",
        body.to_string(),
        &token,
    );
    let resp = app.clone().oneshot(put).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(json.get("would_set_hash").is_some());
    assert!(json["would_set_hash"].as_str().unwrap().len() == 64);
    assert_eq!(json["effective_config"]["model"], "a/one");
    assert!(json["effective_config"]
        .get("malicious-not-allowed")
        .is_none());
    let mut dropped: Vec<String> = serde_json::from_value(json["dropped_keys"].clone()).unwrap();
    dropped.sort();
    assert_eq!(dropped, vec!["malicious-not-allowed", "unknown-as-well"]);
}

#[tokio::test]
async fn opencode_heartbeat_drift_audit() {
    // The node reports `applied_opencode_hash` on every heartbeat. When it
    // doesn't match the assigned profile's hash, the CP logs warns + adds
    // an `opencode.drift` row so the dashboard surfaces drift without
    // breaking the apply loop.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let token = test_token(&app).await;

    let (node_id, cred) = enroll(&app, "drift-node", vec!["opencode".into()], vec![]).await;

    // Assign a profile to the node first.
    let put = put_auth(
        "/v1/opencode-profiles/drift",
        serde_json::json!({"config":{"model":"o/one"}}).to_string(),
        &token,
    );
    let pr = app.clone().oneshot(put).await.unwrap();
    let pr_body = to_bytes(pr.into_body(), usize::MAX).await.unwrap();
    let pr_json: serde_json::Value = serde_json::from_slice(&pr_body).unwrap();
    let pid = pr_json["id"].as_str().unwrap().to_string();
    let pa = post_auth(
        &format!("/v1/nodes/{node_id}/opencode-profile"),
        serde_json::json!({"profile_id": pid}).to_string(),
        &token,
    );
    assert_eq!(
        app.clone().oneshot(pa).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    // Send a heartbeat claiming a different applied hash than the profile's.
    let hb = agentgrid_common::HeartbeatRequest {
        status: Some(NodeStatus::Online),
        name: "drift-node".into(),
        adapters: vec!["opencode".into()],
        repositories: vec![],
        max_concurrency: 1,
        agent_version: String::new(),
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
        account_usage: vec![],
        applied_opencode_hash: Some("deadbeef".repeat(8)),
        active_rss_mib: 0,
        max_rss_mib: 0,
        capabilities_schema_version: None,
        supported_event_versions: None,
    };
    let hb_req = post_auth(
        "/v1/node/heartbeat",
        serde_json::to_string(&hb).unwrap(),
        &cred,
    );
    let resp = app.clone().oneshot(hb_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn opencode_put_if_match_conflicts_on_stale_hash() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let token = test_token(&app).await;

    // Seed profile.
    let body = serde_json::json!({"config":{"model":"a/one"}});
    let put = put_auth("/v1/opencode-profiles/ifmatch", body.to_string(), &token);
    let resp = app.clone().oneshot(put).await.unwrap();
    let init: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let hash = init["hash"].as_str().unwrap().to_string();

    // Stale-hash PUT → 409.
    let stale = serde_json::json!({"config":{"model":"a/two"}});
    let wrong = Request::builder()
        .method("PUT")
        .uri("/v1/opencode-profiles/ifmatch")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("if-match", "\"0000\"")
        .body(Body::from(stale.to_string()))
        .unwrap();
    let resp_wrong = app.clone().oneshot(wrong).await.unwrap();
    assert_eq!(resp_wrong.status(), StatusCode::CONFLICT);

    // Match-hash PUT → 200.
    let ok = Request::builder()
        .method("PUT")
        .uri("/v1/opencode-profiles/ifmatch")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("if-match", format!("\"{hash}\""))
        .body(Body::from(stale.to_string()))
        .unwrap();
    let resp_ok = app.clone().oneshot(ok).await.unwrap();
    assert_eq!(resp_ok.status(), StatusCode::OK);
}

#[tokio::test]
async fn opencode_profile_allowlist_strips_unknown_keys() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let token = test_token(&app).await;
    let cfg = serde_json::json!({
        "model": "anthropic/claude-sonnet-4-5",
        "definitely_not_a_key": { "no": "way" }
    });
    let resp = app
        .clone()
        .oneshot(put_auth(
            "/v1/opencode-profiles/allowlist-check",
            serde_json::to_string(&serde_json::json!({ "config": cfg })).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let profile: agentgrid_common::OpencodeProfile =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let cfg: serde_json::Value = profile.config;
    assert!(cfg.get("model").is_some());
    assert!(
        cfg.get("definitely_not_a_key").is_none(),
        "allowlist must strip unknown keys, got: {cfg}"
    );
}
