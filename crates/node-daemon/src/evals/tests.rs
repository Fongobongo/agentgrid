use crate::evals::{case_command, probe_evals, EvalCase};
use std::time::Duration;

#[test]
fn case_command_parses_plain_single_and_double_quoted() {
    let plain = "id: x\ncommand: cargo test --release\n";
    assert_eq!(case_command(plain).unwrap(), "cargo test --release");
    let single = "id: x\ncommand: 'cargo test'\n";
    assert_eq!(case_command(single).unwrap(), "cargo test");
    let double = "id: x\ncommand: \"cargo test\"\n";
    assert_eq!(case_command(double).unwrap(), "cargo test");
}

#[test]
fn case_command_missing_is_error() {
    assert!(case_command("id: x\n").is_err());
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "ag-evals-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Write a case file and pin it the way `materialize_eval_cases` does:
/// command parsed + content hashed at write time, before any agent ran.
async fn pin_case(dir: &std::path::Path, name: &str, content: &str) -> EvalCase {
    tokio::fs::create_dir_all(dir.join(".agentgrid/evals"))
        .await
        .unwrap();
    let file = dir.join(".agentgrid/evals").join(name);
    tokio::fs::write(&file, content).await.unwrap();
    EvalCase {
        command: case_command(content).unwrap(),
        sha256: agentgrid_common::sha256_hex(content.as_bytes()),
        file,
    }
}

#[tokio::test]
async fn probe_evals_passes_when_no_cases() {
    let dir = temp_dir("empty");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let out = probe_evals(&dir, &[], Duration::from_millis(10))
        .await
        .unwrap();
    assert!(out.ok);
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

// The probe tests below exercise the tamper gate and (on unix) execute cases
// through `sh -c` (the production eval runner's contract — nodes are Linux
// Tier-1 hosts). Unix-only gates mark the ones that need a shell.
#[cfg(unix)]
#[tokio::test]
async fn probe_evals_failing_case_sets_ok_false() {
    // A `false` case fails the suite; output includes the case log tail.
    let dir = temp_dir("fail");
    let case = pin_case(
        &dir,
        "case-false.yaml",
        "id: x\ncommand: \"echo no && exit 1\"\n",
    )
    .await;
    let out = probe_evals(&dir, &[case], Duration::from_secs(5))
        .await
        .unwrap();
    assert!(!out.ok, "failing case must mark the suite failed");
    assert!(out.log.contains("no"), "log keeps the command output");
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[cfg(unix)]
#[tokio::test]
async fn probe_evals_all_passing() {
    let dir = temp_dir("ok");
    let a = pin_case(&dir, "case-a.yaml", "id: a\ncommand: \"echo ok\"\n").await;
    let b = pin_case(&dir, "case-b.yaml", "id: b\ncommand: \"true\"\n").await;
    let out = probe_evals(&dir, &[a, b], Duration::from_secs(5))
        .await
        .unwrap();
    assert!(out.ok, "all-passing suite returns ok=true: {}", out.log);
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn probe_evals_tampered_case_fails_suite_without_running_it() {
    // The agent rewrites a pinned case file after materialisation: the probe
    // must fail the suite and never execute the modified command (the marker
    // file proves the payload did not run). No shell needed — the tamper gate
    // fires before any spawn, so this runs on Windows dev hosts too.
    let dir = temp_dir("tamper");
    let case = pin_case(&dir, "case-a.yaml", "id: a\ncommand: \"true\"\n").await;
    tokio::fs::write(
        dir.join(".agentgrid/evals/case-a.yaml"),
        "id: a\ncommand: \"touch pwned.marker\"\n",
    )
    .await
    .unwrap();
    let out = probe_evals(&dir, &[case], Duration::from_secs(5))
        .await
        .unwrap();
    assert!(!out.ok, "tampered case must fail the suite");
    assert!(
        out.log.contains("changed after materialisation"),
        "log names the tamper: {}",
        out.log
    );
    assert!(
        !dir.join("pwned.marker").exists(),
        "modified command must not execute"
    );
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn probe_evals_deleted_case_fails_suite() {
    let dir = temp_dir("deleted");
    let case = pin_case(&dir, "case-a.yaml", "id: a\ncommand: \"true\"\n").await;
    tokio::fs::remove_file(dir.join(".agentgrid/evals/case-a.yaml"))
        .await
        .unwrap();
    let out = probe_evals(&dir, &[case], Duration::from_secs(5))
        .await
        .unwrap();
    assert!(!out.ok, "deleted pinned case must fail the suite");
    assert!(out.log.contains("unreadable"), "log says why: {}", out.log);
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[cfg(unix)]
#[tokio::test]
async fn probe_evals_ignores_unlisted_agent_written_yaml() {
    // An agent-written `evil.yaml` in the eval dir is never parsed or
    // executed — only CP-stamped, hash-pinned cases run.
    let dir = temp_dir("unlisted");
    let good = pin_case(&dir, "case-a.yaml", "id: a\ncommand: \"echo ok\"\n").await;
    tokio::fs::write(
        dir.join(".agentgrid/evals/evil.yaml"),
        "id: evil\ncommand: \"touch pwned.marker\"\n",
    )
    .await
    .unwrap();
    let out = probe_evals(&dir, &[good], Duration::from_secs(5))
        .await
        .unwrap();
    assert!(out.ok, "unlisted file must not affect the verdict");
    assert!(
        !dir.join("pwned.marker").exists(),
        "unlisted command must not execute"
    );
    let _ = tokio::fs::remove_dir_all(&dir).await;
}
