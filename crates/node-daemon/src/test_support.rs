//! Shared fixtures for the daemon's in-process integration tests.
//!
//! These tests boot a real `agentgrid-control-plane` router in-process and run
//! a transport loop against it, so they all need the same three things: a
//! `Config` that does not touch the operator's directories, a login/enroll pair
//! against that control plane, and a polling wait. They used to live inside
//! `ws.rs`'s test module; the poll-transport tests need them too, and a second
//! copy of a 40-field struct literal is the kind of duplication that silently
//! rots when `Config` gains a field.
//!
//! Everything here is `#[cfg(test)]`-only (the module is declared that way in
//! `main.rs`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{AdapterProtocol, AdapterSpec, Config, Endpoints, SavedCredential, Transport};

/// A `Config` whose every path is under `dir`, with no proxies, no accounts and
/// the mock adapter — enough for a transport loop to start and poll.
pub(crate) fn cfg(server: &str, dir: &std::path::Path) -> Config {
    std::fs::create_dir_all(dir).unwrap();
    Config {
        server: server.to_string(),
        endpoints: Endpoints::new(vec![server.to_string()]),
        node_name: "ws-reconnect-node".into(),
        workspace_root: dir.join("ws"),
        max_concurrency: 1,
        agent_version: "test".into(),
        adapters: vec![AdapterSpec {
            id: "mock".into(),
            protocol: AdapterProtocol::Wrapper,
        }],
        repositories: vec!["*".into()],
        heartbeat_secs: 5,
        enroll_token: None,
        credential_path: dir.join("credential.json"),
        env_file: None,
        repository_root: dir.join("repos"),
        secrets: vec![],
        adapter_env: vec![],
        sandbox: crate::sandbox::SandboxKind::None,
        outbox_root: dir.join("outbox"),
        artifact_spool_root: dir.join("spool"),
        max_artifact_size: 1024 * 1024,
        completion_outbox: Arc::new(
            crate::outbox::CompletionOutbox::open(&dir.join("outbox")).unwrap(),
        ),
        autonomy: Default::default(),
        adapter_versions: Default::default(),
        network_mode: "none".into(),
        transport: Transport::Ws,
        guard_deny: vec![],
        guard_allow: vec![],
        accounts: vec![],
        github_token: None,
        proxies: std::sync::Arc::new(crate::proxy::ProxyPool::new(vec![])),
        managed_adapter_env: Default::default(),
    }
}

/// Log in as the bootstrap user the test control plane seeds.
pub(crate) async fn login(base: &str) -> String {
    reqwest::Client::new()
        .post(format!("{base}/v1/auth/login"))
        .json(&serde_json::json!({"username": "test", "password": "test"}))
        .send()
        .await
        .unwrap()
        .json::<agentgrid_common::LoginResponse>()
        .await
        .unwrap()
        .token
}

/// Mint an enrollment token and enroll one node; returns `(node_id, credential)`.
pub(crate) async fn enroll(base: &str, token: &str, name: &str) -> (String, String) {
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
        .json(&serde_json::json!({
            "token": tok,
            "name": name,
            "adapters": ["mock"],
            "repositories": ["*"],
            "max_concurrency": 1,
        }))
        .send()
        .await
        .unwrap()
        .json::<agentgrid_common::EnrollResponse>()
        .await
        .unwrap();
    (er.node_id, er.credential)
}

/// Node HTTP client carrying the node credential, exactly like `run_transport`
/// builds it — the attempt runner's data-plane calls (events/completion/
/// artifacts) authenticate with this header.
pub(crate) fn authed_client(cred: &SavedCredential) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {}", cred.credential).parse().unwrap(),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

/// Poll `f` until it returns true, panicking with `desc` on timeout.
pub(crate) async fn wait_until<F, Fut>(desc: &str, timeout: Duration, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {desc}");
}
