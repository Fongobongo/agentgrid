use crate::helpers::*;

#[tokio::test]
async fn artifact_upload_and_read() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-art", vec!["mock".into()], vec!["*".into()]).await;
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

    let art = UploadArtifactRequest {
        name: "changes.patch".into(),
        content: "diff --git a/x b/x".into(),
        ..Default::default()
    };
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/artifacts", assign.attempt_id),
            serde_json::to_string(&art).unwrap(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{}/artifacts/changes.patch", assign.task_id),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), b"diff --git a/x b/x".as_slice());

    // Hardening P0 (upstream artifact authorization): a *second* node that is
    // not a workflow consumer of this producer task must NOT be able to fetch
    // its artifact through the node-scoped route. Existence is hidden (404).
    let (_n2, cred2) = enroll(
        &app,
        "node-art-fetcher",
        vec!["mock".into()],
        vec!["*".into()],
    )
    .await;
    let resp = app
        .oneshot(get_auth(
            &format!("/v1/node/tasks/{}/artifacts/changes.patch", assign.task_id),
            &cred2,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "unrelated node cannot read a non-consumer producer artifact"
    );
}

#[tokio::test]
async fn artifact_binary_raw_upload_round_trips() {
    // Stage 2.2: the raw endpoint stores arbitrary bytes + media type + hash,
    // and GET returns them unchanged (would be corrupted via UTF-8 JSON).
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-braw", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:b.txt:b").await;
    let payload: Vec<u8> = vec![0xFF, 0xFE, 0xFD, 0x00, 0x01, 0x02];
    let sha = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&payload);
        let out = h.finalize();
        out.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/v1/node/attempts/{}/artifacts/raw",
                    assign.attempt_id
                ))
                .header("authorization", format!("Bearer {cred}"))
                .header("x-agentgrid-fencing-token", &assign.fencing_token)
                .header("x-artifact-name", "blob.bin")
                .header("x-artifact-media-type", "image/png")
                .header("x-artifact-sha256", sha.clone())
                .body(Body::from(payload.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{}/artifacts/blob.bin", assign.task_id),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "image/png",
        "stored media type must be served back"
    );
    // Hardening P2 item 36: the server-computed hash is exposed for integrity
    // display in the UI.
    assert_eq!(
        resp.headers().get("x-artifact-sha256").unwrap(),
        sha.as_str(),
        "artifact integrity hash surfaced on download"
    );
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), payload.as_slice(), "binary bytes round trip");
}

#[tokio::test]
async fn artifact_download_honors_range_and_streams_partial() {
    // Plan 6.7: the download endpoint must not materialize the artifact in
    // RAM — the response is a bounded file stream that honors a single
    // `Range` (206 + Content-Range + the sliced bytes) and answers an
    // unsatisfiable range with 416 + `bytes */<len>`.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-rng", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:r.txt:r").await;
    ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await;
    let art = UploadArtifactRequest {
        name: "big.log".into(),
        content: "0123456789".into(),
        ..Default::default()
    };
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/artifacts", assign.attempt_id),
            serde_json::to_string(&art).unwrap(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let token = test_token(&app).await;
    let build_req = |range: Option<&str>| {
        let mut b = Request::builder()
            .method("GET")
            .uri(format!("/v1/tasks/{}/artifacts/big.log", assign.task_id))
            .header("authorization", format!("Bearer {token}"));
        if let Some(r) = range {
            b = b.header("range", r);
        }
        b.body(()).unwrap()
    };
    // Full: 200 + accept-ranges.
    let resp = app
        .clone()
        .oneshot(build_req(None).map(|()| Body::empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), b"0123456789");
    // Partial: 206 + slice.
    let resp = app
        .clone()
        .oneshot(build_req(Some("bytes=2-5")).map(|()| Body::empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp.headers().get("content-range").unwrap(), "bytes 2-5/10");
    assert_eq!(resp.headers().get("content-length").unwrap(), "4");
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), b"2345");
    // Suffix: last 4 bytes.
    let resp = app
        .clone()
        .oneshot(build_req(Some("bytes=-4")).map(|()| Body::empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), b"6789");
    // Unsatisfiable: 416 + bytes */len.
    let resp = app
        .clone()
        .oneshot(build_req(Some("bytes=10-")).map(|()| Body::empty()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(resp.headers().get("content-range").unwrap(), "bytes */10");
}

/// Hardening P0 item 3: a successful artifact upload returns the stored
/// metadata (name, size, media type) and the server-computed SHA-256, so a
/// client can verify integrity without a separate GET. Both the JSON and the
/// raw endpoints return the same `ArtifactUploadResponse` body.
#[tokio::test]
async fn artifact_upload_response_carries_metadata_and_hash() {
    use agentgrid_common::ArtifactUploadResponse;
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-meta", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    let payload = b"hello world".to_vec();
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    sha2::Digest::update(&mut h, &payload);
    let expected = sha2::Digest::finalize(h)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    // JSON endpoint.
    let req = UploadArtifactRequest {
        name: "out.txt".into(),
        content: String::from_utf8(payload.clone()).unwrap(),
        media_type: Some("text/plain".into()),
        sha256: None,
    };
    let r = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/artifacts", assign.attempt_id),
            serde_json::to_string(&req).unwrap(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body: ArtifactUploadResponse =
        serde_json::from_slice(&to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body.name, "out.txt");
    assert_eq!(body.size_bytes, payload.len() as i64);
    assert_eq!(body.media_type.as_deref(), Some("text/plain"));
    assert_eq!(body.sha256, expected);

    // Raw endpoint.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/v1/node/attempts/{}/artifacts/raw",
                    assign.attempt_id
                ))
                .header("authorization", format!("Bearer {cred}"))
                .header("x-agentgrid-fencing-token", &assign.fencing_token)
                .header("x-artifact-name", "blob.bin")
                .header("x-artifact-media-type", "image/png")
                .body(Body::from(payload.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body: ArtifactUploadResponse =
        serde_json::from_slice(&to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body.name, "blob.bin");
    assert_eq!(body.size_bytes, payload.len() as i64);
    assert_eq!(body.media_type.as_deref(), Some("image/png"));
    assert_eq!(body.sha256, expected);
}

/// Hardening P0 item 3/36: artifact responses carry a strict CSP
/// (`default-src 'none'`) and CORP `same-origin` so no browser context can
/// execute or cross-read an artifact, even if its media type were sniffed.
#[tokio::test]
async fn artifact_response_has_csp_and_corp() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "n-csp", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    let req = UploadArtifactRequest {
        name: "out.log".into(),
        content: "hello".into(),
        media_type: Some("text/plain".into()),
        sha256: None,
    };
    let r = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/artifacts", assign.attempt_id),
            serde_json::to_string(&req).unwrap(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{}/artifacts/out.log", assign.task_id),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let h = resp.headers();
    assert_eq!(
        h.get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        h.get("content-security-policy")
            .and_then(|v| v.to_str().ok()),
        Some("default-src 'none'; frame-ancestors 'none'")
    );
    assert_eq!(
        h.get("cross-origin-resource-policy")
            .and_then(|v| v.to_str().ok()),
        Some("same-origin")
    );
}

/// Hardening P1 item 15: the artifact storage quota refuses uploads past the
/// cap with 507, and a quota read failure fails CLOSED (503), never open.
/// The quota is captured once at startup (`Limits.artifact_quota_bytes`);
/// tests override it via `set_artifact_quota_bytes`.
#[tokio::test]
async fn artifact_quota_refuses_uploads_over_cap() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "n-quota", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;

    let upload = |body: Vec<u8>| {
        let app = app.clone();
        let cred = cred.clone();
        let aid = assign.attempt_id.clone();
        let fence = assign.fencing_token.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/node/attempts/{aid}/artifacts/raw"))
                    .header("authorization", format!("Bearer {cred}"))
                    .header("x-agentgrid-fencing-token", fence)
                    .header("x-artifact-name", "f.bin")
                    .header("x-artifact-media-type", "application/octet-stream")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        }
    };

    // Unlimited (0): upload succeeds.
    state.set_artifact_quota_bytes(0);
    assert_eq!(upload(b"small".to_vec()).await, StatusCode::OK);

    // Tight 1 KiB quota: any further upload is refused with 507.
    state.set_artifact_quota_bytes(1024);
    let big = vec![0xAAu8; 2 * 1024];
    assert_eq!(
        upload(big).await,
        StatusCode::INSUFFICIENT_STORAGE,
        "quota breach must be 507"
    );
}
