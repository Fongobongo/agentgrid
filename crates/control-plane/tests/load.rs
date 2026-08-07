//! Plan 0.3 stage 0: load harness. Brings up a REAL control-plane HTTP server
//! and drives N mock nodes (enroll → poll → ack → complete) against M tasks,
//! measuring task-creation → assignment latency percentiles plus the write
//! contention counters. `#[ignore]`d — run via `tests/e2e/run-load.sh`, which
//! captures the LOAD-RESULT line for the baseline report.
//!
//! Knobs: AG_LOAD_NODES (default 50), AG_LOAD_TASKS (default 500),
//! AG_LOAD_POLL_MS (default 1000 — the real daemon's poll cadence).

use agentgrid_control_plane::{build_router, AppState};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "load harness; run via tests/e2e/run-load.sh"]
async fn load_baseline_mock_nodes() {
    let nodes_n = env_usize("AG_LOAD_NODES", 50);
    let tasks_n = env_usize("AG_LOAD_TASKS", 500);
    let poll_ms: u64 = std::env::var("AG_LOAD_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);

    // Real server on an ephemeral port (open_temp bootstraps a test/test admin).
    let state = AppState::open_temp().await.unwrap();
    state.store.reconcile_on_startup().await.unwrap();
    state.store.start_maintenance();
    state.store.start_workflow_ticker();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, build_router(state)).await;
    });
    let base = format!("http://{addr}");
    let http = reqwest::Client::new();

    // Admin JWT (test/test from open_temp).
    let token: String = http
        .post(format!("{base}/v1/auth/login"))
        .json(&json!({"username": "test", "password": "test"}))
        .send()
        .await
        .unwrap()
        .json::<agentgrid_common::LoginResponse>()
        .await
        .unwrap()
        .token;

    // Enroll N mock nodes.
    let mut nodes = Vec::with_capacity(nodes_n);
    for i in 0..nodes_n {
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
                "name": format!("load-{i}"),
                "adapters": ["mock"],
                "repositories": ["*"],
                "max_concurrency": 2,
            }))
            .send()
            .await
            .unwrap()
            .json::<agentgrid_common::EnrollResponse>()
            .await
            .unwrap();
        nodes.push(er);
    }

    // Create M tasks, recording creation instants for latency measurement.
    let created: Arc<tokio::sync::Mutex<HashMap<String, Instant>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    for i in 0..tasks_n {
        let t = http
            .post(format!("{base}/v1/tasks"))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({
                "prompt": format!("load task {i}"),
                "repository": "*",
                "adapter": "mock",
                "timeout_secs": 600,
            }))
            .send()
            .await
            .unwrap()
            .json::<agentgrid_common::TaskView>()
            .await
            .unwrap();
        created.lock().await.insert(t.id, Instant::now());
    }
    let start = Instant::now();

    // Node loops: poll → ack → complete; record assignment latency.
    let latencies: Arc<tokio::sync::Mutex<Vec<u128>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut handles = Vec::new();
    for node in nodes {
        let (http, base) = (http.clone(), base.clone());
        let (latencies, completed, created) =
            (latencies.clone(), completed.clone(), created.clone());
        handles.push(tokio::spawn(async move {
            while completed.load(std::sync::atomic::Ordering::Relaxed) < tasks_n {
                let resp = http
                    .post(format!("{base}/v1/node/poll"))
                    .header("authorization", format!("Bearer {}", node.credential))
                    .json(&json!({
                        "node_id": node.node_id,
                        "name": "load",
                        "adapters": ["mock"],
                        "repositories": ["*"],
                        "max_concurrency": 2,
                    }))
                    .send()
                    .await;
                let Ok(resp) = resp else {
                    tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
                    continue;
                };
                let pr = match resp.json::<agentgrid_common::PollResponse>().await {
                    Ok(pr) => pr,
                    Err(_) => {
                        tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
                        continue;
                    }
                };
                let Some(a) = pr.assignment else {
                    tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
                    continue;
                };
                if let Some(t0) = created.lock().await.get(&a.task_id) {
                    latencies.lock().await.push(t0.elapsed().as_millis());
                }
                let fence = format!("Bearer {}", node.credential);
                let _ = http
                    .post(format!("{base}/v1/node/attempts/{}/ack", a.attempt_id))
                    .header("authorization", &fence)
                    .header("x-agentgrid-fencing-token", &a.fencing_token)
                    .send()
                    .await;
                let _ = http
                    .post(format!("{base}/v1/node/attempts/{}/complete", a.attempt_id))
                    .header("authorization", &fence)
                    .header("x-agentgrid-fencing-token", &a.fencing_token)
                    .json(&json!({"exit_code": 0, "pending_artifacts": []}))
                    .send()
                    .await;
                completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }));
    }

    // Wait for the full workload to drain (or fail loudly).
    let deadline = Instant::now() + std::time::Duration::from_secs(600);
    while completed.load(std::sync::atomic::Ordering::Relaxed) < tasks_n {
        assert!(
            Instant::now() < deadline,
            "load did not drain: {}/{tasks_n} completed",
            completed.load(std::sync::atomic::Ordering::Relaxed)
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    for h in handles {
        h.abort();
    }
    let wall = start.elapsed().as_secs_f64();

    // Percentiles.
    let mut lat = latencies.lock().await.clone();
    lat.sort_unstable();
    let pct = |p: f64| lat[(lat.len() as f64 * p).min(lat.len() as f64 - 1.0) as usize];
    let (p50, p99, max) = (pct(0.50), pct(0.99), *lat.last().unwrap_or(&0));

    // Pull the counters from /metrics on the live server.
    let metrics = http.get(format!("{base}/metrics")).send().await.unwrap();
    let m = metrics.text().await.unwrap();
    let metric = |name: &str| -> u64 {
        m.lines()
            .find(|l| l.starts_with(name) && !l.starts_with("#"))
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|v| v.parse::<f64>().ok())
            .map(|v| v as u64)
            .unwrap_or(0)
    };

    println!(
        "LOAD-RESULT nodes={nodes_n} tasks={tasks_n} completed={} wall_s={wall:.1} \
         assign_p50_ms={p50} assign_p99_ms={p99} assign_max_ms={max} \
         write_txns={} write_lock_failures={} poll_requests={} \
         poll_avg_ms={:.2}",
        completed.load(std::sync::atomic::Ordering::Relaxed),
        metric("agentgrid_sqlite_write_txns_total"),
        metric("agentgrid_sqlite_write_lock_failures_total"),
        metric("agentgrid_poll_requests_total"),
        {
            let reqs = metric("agentgrid_poll_requests_total").max(1);
            metric("agentgrid_poll_duration_ms_sum") as f64 / reqs as f64
        }
    );
    assert_eq!(
        metric("agentgrid_sqlite_write_lock_failures_total"),
        0,
        "write-lock contention under the baseline load"
    );
}
