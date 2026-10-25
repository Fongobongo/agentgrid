use crate::helpers::*;

#[tokio::test]
async fn repository_requirements_round_trip_and_reject_invalid() {
    // Plan 6.9: structured requirements store + list back; a typo'd
    // version_req or absurd floor is 400, not silent dead data.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let token = test_token(&app).await;
    let post_repo = |body: serde_json::Value| {
        let app = app.clone();
        let token = token.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/repositories")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };
    // Valid requirements round-trip.
    let resp = post_repo(serde_json::json!({
        "name": "big",
        "git_url": "https://example.com/big.git",
        "default_branch": "main",
        "requirements": {
            "os": "linux",
            "arch": "x86_64",
            "tools": [{"name": "git", "version_req": ">=2.39"}],
            "memory_mb": 2048,
        },
    }))
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let rv: RepositoryView = serde_json::from_slice(&body).unwrap();
    let reqs = rv.requirements.expect("requirements echoed");
    assert_eq!(reqs.os.as_deref(), Some("linux"));
    assert_eq!(reqs.memory_mb, Some(2048));
    assert_eq!(reqs.tools.len(), 1);
    // Listed back identically.
    let resp = app
        .clone()
        .oneshot(get_auth("/v1/repositories", &token))
        .await
        .unwrap();
    let repos: ListResponse<RepositoryView> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let got = repos.items.iter().find(|r| r.name == "big").unwrap();
    assert_eq!(
        got.requirements.as_ref().and_then(|r| r.memory_mb),
        Some(2048)
    );
    // Invalid version_req → 400 with a message.
    let resp = post_repo(serde_json::json!({
        "name": "bad",
        "git_url": "https://example.com/bad.git",
        "default_branch": "main",
        "requirements": {"tools": [{"name": "git", "version_req": "not-a-version!!!"}]},
    }))
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Absurd floor → 400.
    let resp = post_repo(serde_json::json!({
        "name": "bad2",
        "git_url": "https://example.com/bad2.git",
        "default_branch": "main",
        "requirements": {"memory_mb": 999999999},
    }))
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // No requirements → None (legacy shape preserved).
    let resp = post_repo(serde_json::json!({
        "name": "plain",
        "git_url": "https://example.com/plain.git",
        "default_branch": "main",
    }))
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let rv: RepositoryView = serde_json::from_slice(&body).unwrap();
    assert!(rv.requirements.is_none());
}

#[tokio::test]
async fn repository_create_and_list() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let req = CreateRepositoryRequest {
        name: "demo".into(),
        git_url: "https://example.com/demo.git".into(),
        default_branch: "main".into(),
        validation_command: Some("cargo test".into()),
        requirements: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/repositories",
            serde_json::to_string(&req).unwrap(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let rv: RepositoryView = serde_json::from_slice(&body).unwrap();
    assert_eq!(rv.name, "demo");
    assert_eq!(rv.default_branch, "main");

    let resp = app
        .clone()
        .oneshot(get_auth("/v1/repositories", &test_token(&app).await))
        .await
        .unwrap();
    let repos: ListResponse<RepositoryView> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(repos.items.len(), 1);
    assert_eq!(repos.items[0].name, "demo");
}
