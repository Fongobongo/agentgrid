//! Plan 5.3 (#391): every shipped adapter binary answers `--version` with
//! exit 0 and a version token. Per-PR coverage for what the release smoke
//! test asserts per-release; the node capability probe (`probe_adapter`)
//! calls `--version` on these same binaries every heartbeat, so a binary
//! that stopped answering would degrade every node that carries it.

use std::time::Duration;

fn check_version(bin: &str, label: &str) {
    let out = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .unwrap_or_else(|_| panic!("{label}: failed to spawn --version"));
    assert!(
        out.status.success(),
        "{label}: --version must exit 0 (status={:?}, stderr={})",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.split_whitespace().count() >= 2,
        "{label}: --version must print `<name> <version>`, got {text:?}"
    );
    // Second token looks like a version (digits and dots): guards against
    // a wrapper that accidentally runs the agent instead of reporting.
    let version = text.split_whitespace().nth(1).unwrap_or("");
    assert!(
        version.chars().next().is_some_and(|c| c.is_ascii_digit()),
        "{label}: second token must be a version, got {version:?}"
    );
}

#[test]
fn all_adapter_binaries_report_version() {
    // Bound the whole check: a binary that hangs on --version (rather than
    // answering) must fail the test, not the CI job. Each spawn is local
    // and instant; the timeout is a hang-guard only.
    let bins = [
        (env!("CARGO_BIN_EXE_adapter-mock"), "adapter-mock"),
        (env!("CARGO_BIN_EXE_adapter-claude"), "adapter-claude"),
        (env!("CARGO_BIN_EXE_adapter-opencode"), "adapter-opencode"),
        (env!("CARGO_BIN_EXE_adapter-aider"), "adapter-aider"),
        (env!("CARGO_BIN_EXE_adapter-codex"), "adapter-codex"),
        (env!("CARGO_BIN_EXE_adapter-pi"), "adapter-pi"),
    ];
    for (bin, label) in bins {
        let (tx, rx) = std::sync::mpsc::channel();
        let bin = bin.to_string();
        std::thread::spawn(move || {
            check_version(&bin, label);
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| {
                panic!("{label}: --version hung past 30s");
            });
    }
}
