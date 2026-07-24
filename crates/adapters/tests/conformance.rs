//! Conformance suite (Stage 3.2 / plan item «Conformance suite: fixtures for
//! mock/claude/opencode (prepare/start/stream/cancel/collect)»).
//!
//! One parameterized contract exercised against each adapter the suite knows
//! how to launch. Today only `mock` is wired (claude/opencode need real
//! binaries + API keys, covered by `#[ignore]`d tests). The fixture table is
//! the single place to register a new adapter: add a row and the whole contract
//! (start → stream → collect, and start → cancel) runs against it.
//!
//! `CARGO_BIN_EXE_adapter-mock` is only set for integration tests, so this
//! lives here rather than in the lib unit tests.

use agentgrid_adapters::{ExecutionBackend, ProcessBackend, SpawnRequest};
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// One row per adapter the conformance suite can launch. `ignore` rows stay
/// compiled (documenting the expected contract for claude/opencode) but are
/// skipped without a binary/key.
struct AdapterFixture {
    /// Human label for the assertion output.
    name: &'static str,
    /// Adapter binary path (or `bin` for the legacy `ProcessBackend` path).
    bin: String,
    /// A prompt the adapter handles without external services.
    prompt: &'static str,
}

fn mock_fixture() -> AdapterFixture {
    AdapterFixture {
        name: "mock",
        bin: env!("CARGO_BIN_EXE_adapter-mock").to_string(),
        prompt: "write:hello.txt:hi",
    }
}

/// Start → stream → collect: the adapter launches, emits at least one event
/// line referencing its work, and exits 0.
async fn start_stream_collect(f: &AdapterFixture) {
    let req = SpawnRequest {
        bin: f.bin.clone(),
        sandbox_prefix_args: vec![],
        prompt: f.prompt.into(),
        workdir: std::env::temp_dir(),
        attempt_id: format!("attempt-conform-{}", f.name),
        timeout: Duration::from_secs(10),
        env: vec![],
        limits: Default::default(),
    };
    let mut bp = ProcessBackend.spawn(req).unwrap();
    let mut out = String::new();
    bp.stdout.read_to_string(&mut out).await.unwrap();
    let status = bp.child.wait().await.unwrap();
    assert!(status.success(), "{} adapter should exit 0", f.name);
    assert!(
        !out.trim().is_empty(),
        "{} adapter should emit at least one event line",
        f.name
    );
    assert!(
        out.lines().any(|l| l.contains("hello.txt") || l.contains("note:")),
        "{} event stream should mention its work: {out}",
        f.name
    );
}

/// Start → cancel: the adapter launches a long-running prompt turn; killing
/// the process group (the cancel path) must terminate the child without
/// hanging past the timeout. This is the contract the node daemon relies on
/// for `attempt_cancel`.
async fn start_cancel(f: &AdapterFixture) {
    let req = SpawnRequest {
        bin: f.bin.clone(),
        sandbox_prefix_args: vec![],
        // mock `sleep:` blocks a single prompt turn — emulates a long agent
        // run that must be interruptible.
        prompt: "sleep:30".into(),
        workdir: std::env::temp_dir(),
        attempt_id: format!("attempt-cancel-{}", f.name),
        timeout: Duration::from_secs(30),
        env: vec![],
        limits: Default::default(),
    };
    let bp = ProcessBackend.spawn(req).unwrap();
    // Give it a moment to enter the prompt turn.
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Cancel = kill the process group (mirrors node `terminate_group`).
    let pid = bp.child.id().expect("child has a pid");
    unsafe {
        libc::killpg(pid as i32, libc::SIGTERM);
    }
    // Must reap within a short grace; a wedged adapter would hang here.
    let mut child = bp.child;
    let reaped =
        tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    assert!(
        reaped.is_ok(),
        "{} adapter did not terminate within 5s of cancel",
        f.name
    );
}

#[tokio::test]
async fn mock_start_stream_collect() {
    let f = mock_fixture();
    start_stream_collect(&f).await;
}

#[tokio::test]
async fn mock_start_cancel() {
    let f = mock_fixture();
    start_cancel(&f).await;
}

// Future adapters: register a fixture + an `#[ignore]`d test pair. They stay
// compiled (documenting the expected contract) but skip without a binary/key.
// To enable, set the adapter binary path via env and drop `#[ignore]`.

#[tokio::test]
#[ignore = "needs a real claude binary + ANTHROPIC_API_KEY"]
async fn claude_start_stream_collect() {
    let f = AdapterFixture {
        name: "claude",
        bin: std::env::var("AGENTGRID_CLAUDE_BIN").unwrap_or_default(),
        prompt: "say hello in one line",
    };
    if f.bin.is_empty() {
        eprintln!("skipped: AGENTGRID_CLAUDE_BIN unset");
        return;
    }
    start_stream_collect(&f).await;
}

#[tokio::test]
#[ignore = "needs a real opencode binary + API key"]
async fn opencode_start_stream_collect() {
    let f = AdapterFixture {
        name: "opencode",
        bin: std::env::var("AGENTGRID_OPENCODE_BIN").unwrap_or_default(),
        prompt: "say hello in one line",
    };
    if f.bin.is_empty() {
        eprintln!("skipped: AGENTGRID_OPENCODE_BIN unset");
        return;
    }
    start_stream_collect(&f).await;
}
