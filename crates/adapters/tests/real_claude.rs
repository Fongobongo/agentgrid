//! Plan 282: integration test on a real mini-repository through the real
//! `adapter-claude` wrapper + Claude Code CLI (prompt → file on disk →
//! `result` event), not just the spawn-level contract in `conformance.rs`.
//!
//! Gated twice: `#[ignore]` (runs only on explicit request) AND
//! `ANTHROPIC_API_KEY` in the environment — a real LLM call costs money.
//! Run with the key exported:
//! `cargo test -p agentgrid-adapters --test real_claude -- --ignored`.
//! Without the key or the `claude` binary the test reports a skip instead
//! of failing, so `--ignored` on a bare runner stays green.
//!
//! SAFETY: the wrapper only auto-runs tool calls with
//! `--dangerously-skip-permissions` under `AGENTGRID_UNSAFE_UNATTENDED=1`,
//! which this test sets on the CHILD process only — inside a throwaway
//! temp repo it creates and deletes afterwards. Nothing touches the real
//! filesystem or the parent environment.

use std::path::{Path, PathBuf};
use std::time::Duration;

fn git(dir: &Path, args: &[&str]) {
    let st = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git must be installed for the mini-repo fixture");
    assert!(st.success(), "git {args:?} failed");
}

/// Real-LLM mini-repo run: Claude creates one file via the wrapper.
/// Ignored by default; needs `ANTHROPIC_API_KEY` + the `claude` CLI.
#[tokio::test]
#[ignore = "needs a real claude binary + ANTHROPIC_API_KEY (paid LLM call)"]
async fn claude_creates_file_in_mini_repo() {
    let key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    if key.trim().is_empty() {
        eprintln!("skipped: ANTHROPIC_API_KEY unset (paid LLM call, opt-in only)");
        return;
    }
    // The wrapper resolves the CLI via AGENTGRID_CLAUDE_BIN (default `claude`).
    let cli = std::env::var("AGENTGRID_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    if std::process::Command::new(&cli)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        eprintln!("skipped: claude CLI not found ({cli})");
        return;
    }

    // Throwaway mini-repo: one committed base file. Unique per run via
    // pid + nanos (no extra dev-dependency for one temp dir).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir: PathBuf = std::env::temp_dir().join(format!("ag-real-claude-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("base.txt"), "base\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(
        &dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );

    // The real call: tiny prompt, single file, then stop. UNSAFE_UNATTENDED
    // is set on the child only — without it `claude -p` would block on an
    // interactive permission prompt and hit the timeout below.
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_adapter-claude"));
    cmd.arg("--prompt")
        .arg(
            "In the current directory, create a file named hello-claude.txt \
             whose entire content is exactly the line `hi from claude`, then \
             stop. Do not modify any other file and do not run anything else.",
        )
        .current_dir(&dir)
        .env("AGENTGRID_UNSAFE_UNATTENDED", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("adapter-claude must spawn");
    let out = tokio::time::timeout(Duration::from_secs(300), child.wait_with_output())
        .await
        .expect("real claude call must finish within 300s")
        .expect("wait on adapter-claude");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Surface cost/usage lines for the operator running the test.
    for line in stdout.lines().filter(|l| l.contains("usage")) {
        eprintln!("usage: {line}");
    }
    assert!(
        out.status.success(),
        "adapter-claude must exit 0; stderr tail: {}",
        stderr.chars().rev().take(2000).collect::<String>()
    );
    assert!(
        stdout.lines().any(|l| l.contains("\"type\":\"result\"")),
        "event stream must carry a result event; stdout tail: {}",
        stdout.chars().rev().take(1000).collect::<String>()
    );
    let created = std::fs::read_to_string(dir.join("hello-claude.txt"))
        .expect("claude must create hello-claude.txt in the repo");
    assert!(
        created.contains("hi from claude"),
        "file content mismatch: {created:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
