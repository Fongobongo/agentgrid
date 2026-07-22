//! Conformance smoke (Stage 3.2): drive the mock adapter through the
//! `ExecutionBackend` contract and verify start -> stream -> collect works.
//! `CARGO_BIN_EXE_adapter-mock` is only set for integration tests, so this
//! lives here rather than in the lib unit tests.

use agentgrid_adapters::{ExecutionBackend, ProcessBackend, SpawnRequest};
use std::time::Duration;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn mock_adapter_start_stream_collect() {
    let req = SpawnRequest {
        bin: env!("CARGO_BIN_EXE_adapter-mock").to_string(),
        prompt: "write:hello.txt:hi".into(),
        workdir: std::env::temp_dir(),
        attempt_id: "attempt-conform".into(),
        timeout: Duration::from_secs(10),
        env: vec![],
        limits: Default::default(),
    };
    let mut bp = ProcessBackend.spawn(req).unwrap();
    let mut out = String::new();
    bp.stdout.read_to_string(&mut out).await.unwrap();
    let status = bp.child.wait().await.unwrap();
    assert!(status.success(), "mock adapter should exit 0");
    assert!(
        out.lines().any(|l| l.contains("hello.txt")),
        "event stream should mention the written file: {out}"
    );
}
