//! Plan 0.3 2.2: CP WebSocket endpoint acceptance — assignment push < 200 ms,
//! handshake auth, supersede-on-reconnect, cancel push, ack semantics, poll
//! transport coexistence.

use agentgrid_common::ws::NodeWsMsg;
use agentgrid_control_plane::{build_router, AppState};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;

type WsClient = tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Bring up a real server; returns (base_url, admin_jwt).
async fn boot() -> (String, String) {
    let state = AppState::open_temp().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, build_router(state)).await;
    });
    let base = format!("http://{addr}");
    let token = login(&base).await;
    (base, token)
}

async fn login(base: &str) -> String {
    reqwest::Client::new()
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": "test", "password": "test"}))
        .send()
        .await
        .unwrap()
        .json::<agentgrid_common::LoginResponse>()
        .await
        .unwrap()
        .token
}

/// Enroll one node; returns (node_id, credential).
async fn enroll(base: &str, token: &str, name: &str, max_concurrency: u32) -> (String, String) {
    let http = reqwest::Client::new();
    let tok: String = http
        .post(format!("{base}/v1/nodes/enrollment-token"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .json::<agentgrid_common::EnrollTokenResponse>()
        .await
        .unwrap()
        .token;
    let er = http
        .post(format!("{base}/v1/node/enroll"))
        .json(&json!({
            "token": tok,
            "name": name,
            "adapters": ["mock"],
            "repositories": ["*"],
            "max_concurrency": max_concurrency,
        }))
        .send()
        .await
        .unwrap()
        .json::<agentgrid_common::EnrollResponse>()
        .await
        .unwrap();
    (er.node_id, er.credential)
}

async fn ws_connect(base: &str, credential: &str) -> WsClient {
    let url = base.replace("http://", "ws://") + "/v1/node/ws";
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {credential}").parse().unwrap(),
    );
    let (sock, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    sock
}

async fn send_hello(sock: &mut WsClient, node_id: &str, max_concurrency: u32) {
    let hello = NodeWsMsg::Hello {
        node_id: node_id.into(),
        name: "ws-test".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency,
        protocol_version: Some(agentgrid_common::NODE_PROTOCOL_VERSION.into()),
        agent_version: "test".into(),
    };
    sock.send(Message::Text(serde_json::to_string(&hello).unwrap()))
        .await
        .unwrap();
}

/// Next JSON control message within `timeout`, skipping protocol pings.
async fn recv_msg(sock: &mut WsClient, timeout: Duration) -> Option<NodeWsMsg> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, sock.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => return serde_json::from_str(&t).ok(),
            Ok(Some(Ok(_))) => continue, // ping/pong/etc.
            _ => return None,
        }
    }
}

async fn expect_hello_ok(sock: &mut WsClient) {
    match recv_msg(sock, Duration::from_secs(5)).await {
        Some(NodeWsMsg::HelloOk { .. }) => {}
        other => panic!("expected hello_ok, got {other:?}"),
    }
}

async fn create_task(base: &str, token: &str) -> String {
    reqwest::Client::new()
        .post(format!("{base}/v1/tasks"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({
            "prompt": "ws push task",
            "repository": "*",
            "adapter": "mock",
            "timeout_secs": 600,
        }))
        .send()
        .await
        .unwrap()
        .json::<agentgrid_common::TaskView>()
        .await
        .unwrap()
        .id
}

/// The 2.2 acceptance: a queued task reaches a connected WS node < 200 ms
/// after creation (creation does the scheduler notify).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_assignment_pushed_under_200ms_and_ack_lands() {
    let (base, token) = boot().await;
    let (node_id, cred) = enroll(&base, &token, "ws-fast", 2).await;
    let mut sock = ws_connect(&base, &cred).await;
    send_hello(&mut sock, &node_id, 2).await;
    expect_hello_ok(&mut sock).await;

    let t0 = Instant::now();
    let task_id = create_task(&base, &token).await;
    let msg = recv_msg(&mut sock, Duration::from_secs(5)).await;
    let elapsed = t0.elapsed();
    let Some(NodeWsMsg::Assignment { assignments }) = msg else {
        panic!("expected assignment push within 5 s, got {msg:?}")
    };
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].task_id, task_id);
    assert!(
        !assignments[0].fencing_token.is_empty(),
        "WS assignment must carry the fencing token"
    );
    assert!(
        elapsed < Duration::from_millis(200),
        "assignment push took {elapsed:?} (> 200 ms target)"
    );

    // Ack over WS flips the attempt assigned→running (same store path as the
    // HTTP ack endpoint).
    let attempt_id = assignments[0].attempt_id.clone();
    let ack = NodeWsMsg::Ack {
        attempt_ids: vec![attempt_id.clone()],
        ok: true,
        error: None,
    };
    sock.send(Message::Text(serde_json::to_string(&ack).unwrap()))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let st: agentgrid_common::TaskView = reqwest::Client::new()
            .get(format!("{base}/v1/tasks/{task_id}"))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if st.status == agentgrid_common::TaskStatus::Running {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ack never landed (status {:?})",
            st.status
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Bad credential: HTTP 401 at handshake, no upgrade.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_handshake_rejects_bad_credential() {
    let (base, _token) = boot().await;
    let url = base.replace("http://", "ws://") + "/v1/node/ws";
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer not-a-credential".parse().unwrap());
    let err = tokio_tungstenite::connect_async(req).await.unwrap_err();
    let s = err.to_string();
    assert!(s.contains("401"), "expected HTTP 401, got: {s}");
}

/// One connection per node: a second connect closes the first with 4003, and
/// the new connection keeps receiving pushes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_second_connection_supersedes_first() {
    let (base, token) = boot().await;
    let (node_id, cred) = enroll(&base, &token, "ws-dupe", 2).await;
    let mut first = ws_connect(&base, &cred).await;
    send_hello(&mut first, &node_id, 2).await;
    expect_hello_ok(&mut first).await;

    let mut second = ws_connect(&base, &cred).await;
    send_hello(&mut second, &node_id, 2).await;
    expect_hello_ok(&mut second).await;

    // The first socket gets close 4003 (may surface as Close frame or EOF).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "first connection never closed");
        match tokio::time::timeout(Duration::from_secs(2), first.next()).await {
            Ok(Some(Ok(Message::Close(cf)))) => {
                assert_eq!(
                    cf.map(|f| u16::from(f.code)),
                    Some(4003),
                    "supersede close code"
                );
                break;
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) | Ok(None) => break, // socket torn down
            Err(_) => continue,
        }
    }

    // The surviving connection still receives fresh work.
    let _task_id = create_task(&base, &token).await;
    assert!(matches!(
        recv_msg(&mut second, Duration::from_secs(5)).await,
        Some(NodeWsMsg::Assignment { .. })
    ));
}

/// Cancel for a live attempt is pushed to its WS node.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_cancel_pushed_to_owning_node() {
    let (base, token) = boot().await;
    let (node_id, cred) = enroll(&base, &token, "ws-cancel", 2).await;
    let mut sock = ws_connect(&base, &cred).await;
    send_hello(&mut sock, &node_id, 2).await;
    expect_hello_ok(&mut sock).await;
    let task_id = create_task(&base, &token).await;
    let Some(NodeWsMsg::Assignment { assignments }) =
        recv_msg(&mut sock, Duration::from_secs(5)).await
    else {
        panic!("no assignment")
    };
    let attempt_id = assignments[0].attempt_id.clone();

    reqwest::Client::new()
        .post(format!("{base}/v1/tasks/{task_id}/cancel"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    match recv_msg(&mut sock, Duration::from_secs(5)).await {
        Some(NodeWsMsg::Cancel { attempt_id: id }) => assert_eq!(id, attempt_id),
        other => panic!("expected cancel push, got {other:?}"),
    }
}

/// Poll-based nodes keep working while a WS node is connected (identical
/// scheduling semantics, N/N-1). Both nodes have one slot: the WS node takes
/// the first task by push, the poll node takes the second by long-poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn poll_node_and_ws_node_coexist() {
    let (base, token) = boot().await;
    let (ws_node, ws_cred) = enroll(&base, &token, "co-ws", 1).await;
    let (poll_node, poll_cred) = enroll(&base, &token, "co-poll", 1).await;

    let mut sock = ws_connect(&base, &ws_cred).await;
    send_hello(&mut sock, &ws_node, 1).await;
    expect_hello_ok(&mut sock).await;

    // Task 1: the WS node (the only one with free capacity on a push path)
    // receives it via push.
    let t1 = create_task(&base, &token).await;
    let Some(NodeWsMsg::Assignment { assignments }) =
        recv_msg(&mut sock, Duration::from_secs(5)).await
    else {
        panic!("ws node got no push for task 1")
    };
    assert_eq!(assignments[0].task_id, t1);

    // Task 2: the WS node is at capacity, so the poll node gets it.
    let t2 = create_task(&base, &token).await;
    let resp: agentgrid_common::PollResponse = reqwest::Client::new()
        .post(format!("{base}/v1/node/poll"))
        .header("authorization", format!("Bearer {poll_cred}"))
        .json(&json!({
            "node_id": poll_node,
            "name": "co-poll",
            "adapters": ["mock"],
            "repositories": ["*"],
            "max_concurrency": 1,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let poll_assignment = resp.assignment.expect("poll node must get task 2");
    assert_eq!(poll_assignment.task_id, t2);
}
