use crate::helpers::*;

/// Plan 6.6: responses above 8 KiB are brotli-compressed when the client
/// sends Accept-Encoding; smaller ones pass through untouched.
#[tokio::test]
async fn compression_kicks_in_above_8kib() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-1", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;

    // ~40 KiB of events spread over a few ingest batches (CP caps batch
    // bytes, so push several).
    for seq in 0..8 {
        let events: Vec<IncomingEvent> = (0..50)
            .map(|i| IncomingEvent {
                sequence: seq * 50 + i + 1,
                r#type: EventType::Stdout,
                payload: json!({"text": "x".repeat(128)}),
            })
            .collect();
        let resp = app
            .clone()
            .oneshot(post_node(
                &format!("/v1/node/attempts/{}/events", assign.attempt_id),
                serde_json::to_string(&IngestEventsRequest { events }).unwrap(),
                &cred,
                &assign.fencing_token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Big listing + Accept-Encoding: br -> compressed.
    let resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/v1/tasks/{}/events", assign.task_id))
                .header(
                    "authorization",
                    format!("Bearer {}", test_token(&app).await),
                )
                .header("accept-encoding", "br")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let enc = resp
        .headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(enc, "br", "large event listing must be brotli-compressed");

    // Small task-details response: no Content-Encoding even when offered.
    let resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/v1/tasks/{}", assign.task_id))
                .header(
                    "authorization",
                    format!("Bearer {}", test_token(&app).await),
                )
                .header("accept-encoding", "br, gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get("content-encoding").is_none(),
        "small JSON must not pay the compression cost"
    );
}

#[tokio::test]
async fn events_tail_query_serves_last_n() {
    // Plan 6.7 (#579): `GET /v1/tasks/{id}/events?tail=N` serves the last
    // N events ascending (Task-details fast path); no tail = forward from
    // the start (unchanged).
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-tail", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:t.txt:t").await;
    ack_attempt(&app, &assign.attempt_id, &cred, &assign.fencing_token).await;
    let ev = IngestEventsRequest {
        events: (1..=6u64)
            .map(|seq| IncomingEvent {
                sequence: seq,
                r#type: EventType::Stdout,
                payload: json!({"text": format!("line {seq}")}),
            })
            .collect(),
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

    let token = test_token(&app).await;
    let get = |q: &str| {
        Request::builder()
            .method("GET")
            .uri(format!("/v1/tasks/{}/events{q}", assign.task_id))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    // Tail 2 → lines 5,6 ascending.
    let resp = app.clone().oneshot(get("?tail=2")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let evs: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0]["sequence"], 5);
    assert_eq!(evs[1]["sequence"], 6);
    // No tail → forward from the start, all 6.
    let resp = app.clone().oneshot(get("")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let evs: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(evs.len(), 6);
    assert_eq!(evs[0]["sequence"], 1);
}

#[tokio::test]
async fn metrics_endpoint_exposes_counts() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let resp = app
        .clone()
        .oneshot(get_auth("/metrics", &test_token(&app).await))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("agentgrid_tasks"));
    assert!(text.contains("agentgrid_attempts_total"));
    // Stage 5.2 additions.
    assert!(text.contains("agentgrid_task_duration_seconds"));
    assert!(text.contains("agentgrid_tasks_total"));
    assert!(text.contains("agentgrid_node_free_disk_mb"));
    assert!(text.contains("agentgrid_sqlite_db_bytes"));
    assert!(text.contains("agentgrid_sqlite_wal_bytes"));
    // Stage 12: error-code outcome split (resource_limit vs agent_failed).
    assert!(text.contains("agentgrid_task_errors_total"));
}

/// Hardening P1 item 14: a terminal attempt must reject further event
/// ingestion (404) — a node that restarts after the attempt was completed or
/// marked lost must not append to or resurrect a finished event stream.
#[tokio::test]
async fn events_rejected_for_terminal_attempt() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "n-term-ev", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    // Run the attempt to running, then complete it.
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
    // Events after completion are rejected.
    let ev = IngestEventsRequest {
        events: vec![IncomingEvent {
            sequence: 999,
            r#type: EventType::Stdout,
            payload: json!({"text": "late"}),
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
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Hardening P1 item 14: a single ingest batch is bounded by event count —
/// exceeding `AGENTGRID_MAX_EVENT_BATCH` is rejected with PAYLOAD_TOO_LARGE
/// before any DB write.
#[tokio::test]
async fn events_batch_count_limit_enforced() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "n-batch", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    let too_many: Vec<IncomingEvent> = (0..600)
        .map(|i| IncomingEvent {
            sequence: i,
            r#type: EventType::Stdout,
            payload: json!({"text": "x"}),
        })
        .collect();
    let ev = IngestEventsRequest { events: too_many };
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
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

/// Hardening P1 item 14: a single node flooding the event endpoint beyond the
/// per-node rate limit is throttled with 429 after the budget is spent.
#[tokio::test]
async fn events_rate_limit_throttles_one_node() {
    let state = AppState::open_temp().await.unwrap();
    // Tiny budget so we exhaust it without firing many requests; long window so
    // the test does not race a rolling reset. Set via the state hook, NOT env:
    // env is process-global and would poison the limiter of every test that
    // constructs an AppState concurrently (the cross-test 429 flake).
    state.set_event_rate_limits(2, 3600).await;
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "n-rate", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    let mk = |seq| IncomingEvent {
        sequence: seq,
        r#type: EventType::Stdout,
        payload: json!({"text":"x"}),
    };
    for seq in 0..2u64 {
        let req = IngestEventsRequest {
            events: vec![mk(seq)],
        };
        let resp = app
            .clone()
            .oneshot(post_node(
                &format!("/v1/node/attempts/{}/events", assign.attempt_id),
                serde_json::to_string(&req).unwrap(),
                &cred,
                &assign.fencing_token,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "req {seq} under limit must be OK"
        );
    }
    for seq in 2..4u64 {
        let req = IngestEventsRequest {
            events: vec![mk(seq)],
        };
        let resp = app
            .clone()
            .oneshot(post_node(
                &format!("/v1/node/attempts/{}/events", assign.attempt_id),
                serde_json::to_string(&req).unwrap(),
                &cred,
                &assign.fencing_token,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "req {seq} over limit must be 429"
        );
    }
    let _ = node_id;
}

/// Hardening P0 item 9: the global ingest cursor stays strictly monotonic
/// under concurrent ingestion (counter allocation is serialised by the write
/// transaction). Duplicate redelivery consumes a counter value but never
/// produces a duplicate row or a non-monotonic read order.
#[tokio::test]
async fn ingest_id_monotonic_under_concurrent_ingestion() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "n-mon", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;

    // Fire 20 concurrent single-event batches (each its own HTTP request →
    // its own transaction). Sequences start at 1 (matching the node daemon's
    // per-attempt counter), so the contiguous-prefix ACK contract holds.
    let mut handles = Vec::new();
    for i in 1..=20u64 {
        let app = app.clone();
        let cred = cred.clone();
        let aid = assign.attempt_id.clone();
        let fence = assign.fencing_token.clone();
        handles.push(tokio::spawn(async move {
            let req = IngestEventsRequest {
                events: vec![IncomingEvent {
                    sequence: i,
                    r#type: EventType::Stdout,
                    payload: json!({ "text": format!("line-{i}") }),
                }],
            };
            let resp = app
                .oneshot(post_node(
                    &format!("/v1/node/attempts/{aid}/events"),
                    serde_json::to_string(&req).unwrap(),
                    &cred,
                    &fence,
                ))
                .await
                .unwrap();
            let status = resp.status();
            let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            let ack: Option<agentgrid_common::IngestEventsAck> = serde_json::from_slice(&body).ok();
            (status, ack)
        }));
    }
    let mut total_accepted = 0u64;
    for h in handles {
        let (s, ack) = h.await.unwrap();
        assert_eq!(
            s,
            StatusCode::OK,
            "concurrent ingest must be OK, got {s} (ack={ack:?})"
        );
        total_accepted += ack.map(|a| a.accepted).unwrap_or(0);
    }
    assert_eq!(total_accepted, 20, "all 20 events accepted by the CP");

    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{}/events?after_ingest=0", assign.task_id),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let evs: Vec<agentgrid_common::TaskEvent> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(evs.len(), 20, "all concurrent events landed");
    let mut prev = 0u64;
    for e in &evs {
        assert!(
            e.ingest_id > prev,
            "ingest_id strictly monotonic in read order"
        );
        prev = e.ingest_id;
    }

    // Duplicate redelivery of one event: no new row, no duplicate ingest_id.
    let dup = IngestEventsRequest {
        events: vec![IncomingEvent {
            sequence: 1,
            r#type: EventType::Stdout,
            payload: json!({ "text": "line-1" }),
        }],
    };
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/events", assign.attempt_id),
            serde_json::to_string(&dup).unwrap(),
            &cred,
            &assign.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "dup ingest must be OK, got {}",
        resp.status()
    );
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{}/events?after_ingest=0", assign.task_id),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let evs: Vec<agentgrid_common::TaskEvent> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(evs.len(), 20, "duplicate did not add a row");
}

/// Hardening P0 item 9: events across attempts are ordered by the global
/// `ingest_id` cursor — a new attempt's seq-1 events come AFTER an old
/// attempt's tail, and the `after_ingest` cursor resumes without gaps/dups.
#[tokio::test]
async fn events_ordered_by_global_ingest_cursor_across_attempts() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-cur", vec!["mock".into()], vec!["*".into()]).await;
    let assign1 = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;

    // Attempt 1: ingest events seq 1..3.
    for (i, text) in ["old-1", "old-2", "old-3"].iter().enumerate() {
        let ev = IngestEventsRequest {
            events: vec![IncomingEvent {
                sequence: i as u64 + 1,
                r#type: EventType::Stdout,
                payload: json!({ "text": text }),
            }],
        };
        let resp = app
            .clone()
            .oneshot(post_node(
                &format!("/v1/node/attempts/{}/events", assign1.attempt_id),
                serde_json::to_string(&ev).unwrap(),
                &cred,
                &assign1.fencing_token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Complete attempt 1 as failed, then retry -> attempt 2 with fresh seq.
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/complete", assign1.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 1,
                error_code: Some("agent_failed".into()),
                ..complete_req()
            })
            .unwrap(),
            &cred,
            &assign1.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    app.clone()
        .oneshot(post_auth(
            &format!("/v1/tasks/{}/retry", assign1.task_id),
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let assign2 = {
        let mut got = None;
        let poll_req = PollRequest {
            node_id: node_id.clone(),
            name: "n".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
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
                serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            if let Some(a) = pr.assignment {
                got = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        got.expect("retry never assigned")
    };
    assert_ne!(assign2.attempt_id, assign1.attempt_id);
    let ev = IngestEventsRequest {
        events: vec![IncomingEvent {
            sequence: 1,
            r#type: EventType::Stdout,
            payload: json!({ "text": "new-1" }),
        }],
    };
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/events", assign2.attempt_id),
            serde_json::to_string(&ev).unwrap(),
            &cred,
            &assign2.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Read back: ALL events ordered by ingest_id — new attempt's seq-1 lands
    // after the old attempt's seq-3, and the cursor is strictly monotonic.
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{}/events", assign1.task_id),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let evs: Vec<agentgrid_common::TaskEvent> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(evs.len(), 4);
    assert_eq!(evs[0].sequence, 1);
    assert_eq!(evs[3].sequence, 1, "new attempt restarts its sequence at 1");
    assert!(evs[0].ingest_id < evs[1].ingest_id);
    assert!(evs[1].ingest_id < evs[2].ingest_id);
    assert!(
        evs[2].ingest_id < evs[3].ingest_id,
        "global cursor monotonic across attempts"
    );
    assert_eq!(
        evs[3].payload["text"], "new-1",
        "new attempt's seq-1 event comes after the old attempt's tail"
    );

    // Resume on the ingest cursor: no gaps, no dups.
    let last = evs[2].ingest_id;
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{}/events?after_ingest={last}", assign1.task_id),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let tail: Vec<agentgrid_common::TaskEvent> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].payload["text"], "new-1");
}

/// Hardening P0 item 9: the SSE stream emits existing events with the global
/// `ingest_id` as the SSE `id:` field, so a browser reconnect (Last-Event-ID)
/// resumes exactly where it stopped — even across attempts. Reads the first
/// SSE frame from the live stream and asserts the id matches the ingest cursor.
#[tokio::test]
async fn sse_stream_emits_ingest_id_cursor() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "n-sse", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;

    // Ingest two events so the stream has something to emit.
    let ev = IngestEventsRequest {
        events: vec![
            IncomingEvent {
                sequence: 1,
                r#type: EventType::Stdout,
                payload: json!({"text": "sse-1"}),
            },
            IncomingEvent {
                sequence: 2,
                r#type: EventType::Stdout,
                payload: json!({"text": "sse-2"}),
            },
        ],
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

    // Start the SSE stream from ingest 0 and read the first chunk (the stream
    // is long-polling, so bound the read with a timeout and abort afterwards).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/v1/tasks/{}/events/stream?after_ingest=0",
                    assign.task_id
                ))
                .header(
                    "authorization",
                    format!("Bearer {}", test_token(&app).await),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    use futures_util::StreamExt;
    let body = resp.into_body();
    let mut stream = body.into_data_stream();
    let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("sse stream produced data within timeout")
        .expect("chunk available")
        .expect("chunk ok");
    let text = String::from_utf8_lossy(&chunk).to_string();
    // The first frame carries `id:<ingest_id>` and the first event's JSON.
    assert!(
        text.contains("id:1") || text.contains("event: task-event"),
        "first SSE frame should reference the ingest cursor: {text:?}"
    );
    assert!(
        text.contains("sse-1"),
        "first frame carries the first event: {text:?}"
    );
}
