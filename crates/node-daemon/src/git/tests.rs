//! Test suite for ../git.rs (extracted during the node-daemon split).

use super::*;
use agentgrid_common::Assignment;

fn make_assignment(git_url: &str, default_branch: &str) -> Assignment {
    Assignment {
        attempt_id: "attempt-test".into(),
        fencing_token: String::new(),
        task_id: "task-test".into(),
        repository: "repo".into(),
        prompt: "x".into(),
        adapter: "mock".into(),
        number: 1,
        timeout_secs: 60,
        git_url: git_url.into(),
        default_branch: default_branch.into(),
        validation_command: None,
        validation_timeout_secs: None,
        base_commit: None,
        parent_acp_session_id: None,
        network_mode: None,
        provenance: None,
        upstream_commits: vec![],
        upstream_task_ids: vec![],
        group_id: None,
        read_only: false,
        eval_cases: vec![],
        consensus_group_id: None,
        consensus_member: None,
        opencode_override: None,
        github_push: false,
        github_repo: None,
        github_issue: None,
        github_base_ref: None,
    }
}

#[test]
fn plain_dir_has_no_commit() {
    let dir = std::env::temp_dir().join(format!("ag-git-plain-{}", uuid::Uuid::new_v4()));
    let ws_root = dir.join("ws");
    let a = make_assignment("", "main");
    let ws = prepare_workspace(&dir.join("repos"), &ws_root, &a, &[], &[]).unwrap();
    assert!(!ws.is_git);
    assert!(ws.path.exists());
    assert!(finalize_workspace(ws, "n@x").unwrap().is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn worktree_commit_and_patch() {
    let dir = std::env::temp_dir().join(format!("ag-git-{}", uuid::Uuid::new_v4()));
    let origin = dir.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]).unwrap();
    std::fs::write(origin.join("base.txt"), "base").unwrap();
    git(&origin, &["add", "-A"]).unwrap();
    git(
        &origin,
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
    )
    .unwrap();

    let a = make_assignment(origin.to_str().unwrap(), "main");
    let ws = prepare_workspace(&dir.join("repos"), &dir.join("ws"), &a, &[], &[]).unwrap();
    assert!(ws.is_git);
    // Agent writes a new file in the worktree.
    std::fs::write(ws.path.join("new.txt"), "hello").unwrap();

    let patch_path = ws.path.join("changes.patch");
    let sha = finalize_workspace(ws, "agent@agentgrid").unwrap();
    assert!(sha.is_some());
    let patch = std::fs::read_to_string(&patch_path).unwrap();
    assert!(patch.contains("new.txt"), "patch missing new file: {patch}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cleanup_workspace_removes_worktree_and_branch() {
    let dir = std::env::temp_dir().join(format!("ag-git-cleanup-{}", uuid::Uuid::new_v4()));
    let origin = dir.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]).unwrap();
    std::fs::write(origin.join("base.txt"), "base").unwrap();
    git(&origin, &["add", "-A"]).unwrap();
    git(
        &origin,
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
    )
    .unwrap();
    let a = make_assignment(origin.to_str().unwrap(), "main");
    let ws = prepare_workspace(&dir.join("repos"), &dir.join("ws"), &a, &[], &[]).unwrap();
    assert!(ws.is_git);
    let ws_path = ws.path.clone();
    let repo_dir = ws.repo_dir.clone().unwrap();
    let branch = ws.branch.clone().unwrap();
    finalize_workspace(ws, "agent@agentgrid").unwrap();
    assert!(ws_path.exists(), "worktree dir should exist before cleanup");
    let branches_before = git_out(&repo_dir, &["branch", "--list"]).unwrap();
    assert!(
        branches_before.contains(&branch),
        "branch missing: {branches_before}"
    );
    cleanup_workspace(&ws_path, Some(&repo_dir), Some(&branch));
    assert!(
        !ws_path.exists(),
        "worktree dir should be gone after cleanup"
    );
    let branches_after = git_out(&repo_dir, &["branch", "--list"]).unwrap();
    assert!(
        !branches_after.contains(&branch),
        "branch should be gone: {branches_after}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cleanup_workspace_plain_dir_no_git() {
    let dir = std::env::temp_dir().join(format!("ag-git-cleanup-plain-{}", uuid::Uuid::new_v4()));
    let a = make_assignment("", "main");
    let ws = prepare_workspace(&dir.join("repos"), &dir.join("ws"), &a, &[], &[]).unwrap();
    assert!(!ws.is_git);
    let ws_path = ws.path.clone();
    assert!(ws_path.exists());
    cleanup_workspace(&ws_path, None, None);
    assert!(!ws_path.exists(), "plain dir should be gone after cleanup");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn prune_stale_workspaces_removes_old_keeps_fresh() {
    let dir = std::env::temp_dir().join(format!("ag-prune-{}", uuid::Uuid::new_v4()));
    let ws_root = dir.join("ws");
    let repos = dir.join("repos");
    std::fs::create_dir_all(&ws_root).unwrap();
    // Stale dir: created now, but a 0s retention prunes everything older
    // than 0 (i.e. mtime < now). Backdate by recreating after a short sleep
    // so its mtime is strictly in the past relative to the cutoff.
    let stale = ws_root.join("old-attempt");
    std::fs::create_dir_all(&stale).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1200));
    // Fresh dir: created right before prune, mtime is now.
    let fresh = ws_root.join("fresh-attempt");
    std::fs::create_dir_all(&fresh).unwrap();
    // retention = 1s: `stale` (mtime ~1.2s ago) is older → pruned;
    // `fresh` (mtime ~0s ago) is newer → kept.
    prune_stale_workspaces(&ws_root, &repos, std::time::Duration::from_secs(1));
    assert!(!stale.exists(), "stale dir should be pruned");
    assert!(fresh.exists(), "fresh dir should be kept");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn base_commit_pins_worktree_to_commit() {
    let dir = std::env::temp_dir().join(format!("ag-git-base-{}", uuid::Uuid::new_v4()));
    let origin = dir.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]).unwrap();
    std::fs::write(origin.join("a.txt"), "a").unwrap();
    git(&origin, &["add", "-A"]).unwrap();
    git(
        &origin,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-q",
            "-m",
            "c0",
        ],
    )
    .unwrap();
    let c0 = git_out(&origin, &["rev-parse", "HEAD"]).unwrap();
    // a second commit so the default branch tip != base_commit
    std::fs::write(origin.join("b.txt"), "b").unwrap();
    git(&origin, &["add", "-A"]).unwrap();
    git(
        &origin,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-q",
            "-m",
            "c1",
        ],
    )
    .unwrap();

    let mut a = make_assignment(origin.to_str().unwrap(), "main");
    a.base_commit = Some(c0.clone());
    let ws = prepare_workspace(&dir.join("repos"), &dir.join("ws"), &a, &[], &[]).unwrap();
    assert!(ws.is_git);
    assert_eq!(ws.base_commit.as_deref(), Some(c0.as_str()));
    // worktree HEAD is the pinned commit, not the main tip
    let head = git_out(&ws.path, &["rev-parse", "HEAD"]).unwrap();
    assert_eq!(head, c0);
    // the agent's new file is diffed relative to base_commit
    std::fs::write(ws.path.join("new.txt"), "hello").unwrap();
    let patch_path = ws.path.join("changes.patch");
    let sha = finalize_workspace(ws, "agent@agentgrid").unwrap();
    assert!(sha.is_some());
    let patch = std::fs::read_to_string(&patch_path).unwrap();
    assert!(patch.contains("new.txt"), "patch missing new file: {patch}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn raw_and_validation_logs_excluded_from_commit_and_patch() {
    // Stage 2.2: agent-side logs living inside the worktree (raw mirror,
    // validation output) must never leak into the committed diff / patch.
    let dir = std::env::temp_dir().join(format!("ag-git-leak-{}", uuid::Uuid::new_v4()));
    let origin = dir.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]).unwrap();
    std::fs::write(origin.join("base.txt"), "base").unwrap();
    git(&origin, &["add", "-A"]).unwrap();
    git(
        &origin,
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
    )
    .unwrap();

    let a = make_assignment(origin.to_str().unwrap(), "main");
    let ws = prepare_workspace(&dir.join("repos"), &dir.join("ws"), &a, &[], &[]).unwrap();
    // Agent writes a legit change plus the private logs node writes in-tree.
    std::fs::write(ws.path.join("new.txt"), "hello").unwrap();
    std::fs::write(ws.path.join("agent-raw-output.log"), "SECRET-RAW").unwrap();
    std::fs::write(ws.path.join("validation.log"), "SECRET-VAL").unwrap();

    let patch_path = ws.path.join("changes.patch");
    let sha = finalize_workspace(ws, "agent@agentgrid").unwrap();
    assert!(sha.is_some());
    let patch = std::fs::read_to_string(&patch_path).unwrap();
    assert!(
        patch.contains("new.txt"),
        "legit change missing from patch: {patch}"
    );
    assert!(
        !patch.contains("agent-raw-output.log"),
        "raw log leaked into patch: {patch}"
    );
    assert!(
        !patch.contains("validation.log"),
        "validation log leaked into patch: {patch}"
    );
    assert!(
        !patch.contains("SECRET"),
        "secret leaked into patch: {patch}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn parallel_prep_same_repo_does_not_race() {
    // Stage 2.3: two concurrent attempts of one repository must not corrupt
    // the shared clone (fetch / checkout -B / worktree add serialize per repo).
    let dir = std::env::temp_dir().join(format!("ag-git-par-{}", uuid::Uuid::new_v4()));
    let origin = dir.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]).unwrap();
    std::fs::write(origin.join("base.txt"), "base").unwrap();
    git(&origin, &["add", "-A"]).unwrap();
    git(
        &origin,
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
    )
    .unwrap();

    let repos = dir.join("repos");
    let ws_root = dir.join("ws");
    let url = origin.to_str().unwrap().to_string();
    let mut handles = vec![];
    for n in 0..4u32 {
        let repos = repos.clone();
        let ws_root = ws_root.clone();
        let url = url.clone();
        handles.push(std::thread::spawn(move || {
            let a = Assignment {
                attempt_id: format!("att-{n}"),
                fencing_token: String::new(),
                task_id: format!("task-{n}"),
                repository: "repo".into(),
                prompt: "x".into(),
                adapter: "mock".into(),
                number: 1,
                timeout_secs: 60,
                git_url: url,
                default_branch: "main".into(),
                validation_command: None,
                validation_timeout_secs: None,
                base_commit: None,
                parent_acp_session_id: None,
                network_mode: None,
                provenance: None,
                upstream_commits: vec![],
                upstream_task_ids: vec![],
                group_id: None,
                read_only: false,
                eval_cases: vec![],
                consensus_group_id: None,
                consensus_member: None,
                opencode_override: None,
                github_push: false,
                github_repo: None,
                github_issue: None,
                github_base_ref: None,
            };
            prepare_workspace(&repos, &ws_root, &a, &[], &[])
        }));
    }
    let mut ok = 0;
    let mut paths = std::collections::HashSet::new();
    for h in handles {
        if let Ok(ws) = h.join().unwrap() {
            assert!(ws.is_git, "worktree should be a git worktree");
            assert!(ws.path.exists(), "worktree path must exist");
            // Stage 7.2: each parallel ready step gets its own worktree
            // — paths must be distinct (no attempt reuses another).
            assert!(paths.insert(ws.path.clone()), "duplicate worktree path");
            ok += 1;
        }
    }
    assert_eq!(ok, 4, "all parallel prepares must succeed");
    assert_eq!(paths.len(), 4, "four distinct worktrees");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn integrator_cherry_picks_nonconflicting_worker_commits() {
    // Stage 8 / line 239: an integrator step's worktree lands each
    // upstream worker's winning commit before the agent runs. We model
    // two non-conflicting worker commits on separate worker branches off
    // `origin/main`; the integrator's `upstream_commits` lists their SHAs,
    // and the prepared worktree must contain both files (both commits
    // cherry-picked onto the integrator branch).
    let dir = std::env::temp_dir().join(format!("ag-git-integ-{}", uuid::Uuid::new_v4()));
    let origin = dir.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]).unwrap();
    std::fs::write(origin.join("base.txt"), "base").unwrap();
    git(&origin, &["add", "-A"]).unwrap();
    git(
        &origin,
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
    )
    .unwrap();

    // Worker 1: add file1 on its own branch off main.
    git(&origin, &["checkout", "-q", "-b", "worker1"]).unwrap();
    std::fs::write(origin.join("file1.txt"), "one").unwrap();
    git(
        &origin,
        &["-c", "user.name=t", "-c", "user.email=t@x", "add", "-A"],
    )
    .unwrap();
    git(
        &origin,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-q",
            "-m",
            "worker1",
        ],
    )
    .unwrap();
    let sha1 = git_out(&origin, &["rev-parse", "worker1"]).unwrap();

    // Worker 2: non-conflicting edit on its own branch off main.
    git(&origin, &["checkout", "-q", "main"]).unwrap();
    git(&origin, &["checkout", "-q", "-b", "worker2"]).unwrap();
    std::fs::write(origin.join("file2.txt"), "two").unwrap();
    git(
        &origin,
        &["-c", "user.name=t", "-c", "user.email=t@x", "add", "-A"],
    )
    .unwrap();
    git(
        &origin,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-q",
            "-m",
            "worker2",
        ],
    )
    .unwrap();
    let sha2 = git_out(&origin, &["rev-parse", "worker2"]).unwrap();

    // Integrator prep with both worker SHAs.
    let a = make_assignment(origin.to_str().unwrap(), "main");
    let ws = prepare_workspace(&dir.join("repos"), &dir.join("ws"), &a, &[sha1, sha2], &[])
        .expect("non-conflicting cherry-picks succeed");
    assert!(ws.path.join("file1.txt").exists(), "worker1 commit landed");
    assert!(ws.path.join("file2.txt").exists(), "worker2 commit landed");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn integrator_applies_patch_bundle_when_sha_not_reachable() {
    // Stage 8 / line 257: distributed workflow without a shared Git remote
    // — an upstream worker's commit SHA may not be reachable via
    // `git fetch origin <sha>` on the integrator's node (no shared
    // remote, or the sha lives on a different host). The node falls back
    // to the worker's `changes.patch` artifact fetched from the control
    // plane and `git apply --3way`s it onto the worktree. Modeled here:
    // we hand `prepare_workspace` a SHA that does not exist anywhere in
    // the local mirror (git fetch + cat-file -e both miss) together with
    // a binary patch keyed by that same SHA — the prepared worktree must
    // end up containing the patched file.
    let dir = std::env::temp_dir().join(format!("ag-git-patch-{}", uuid::Uuid::new_v4()));
    let origin = dir.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]).unwrap();
    std::fs::write(origin.join("base.txt"), "base").unwrap();
    git(&origin, &["add", "-A"]).unwrap();
    git(
        &origin,
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
    )
    .unwrap();

    // Build a `changes.patch` that adds a new file (`worker.txt`). The
    // "winning" SHA below does not exist in the local mirror, so neither
    // `git fetch origin <sha>` nor `git cat-file -e <sha>` resolves.
    let patch =
        b"diff --git a/worker.txt b/worker.txt\nnew file mode 100644\nindex 0000000..257cc56\n--- /dev/null\n+++ b/worker.txt\n@@ -0,0 +1 @@\n+worker change\n";
    let fake_sha = "1111111111111111111111111111111111111111".to_string();

    let a = make_assignment(origin.to_str().unwrap(), "main");
    let shas = vec![fake_sha.clone()];
    let ws = prepare_workspace(
        &dir.join("repos"),
        &dir.join("ws"),
        &a,
        &shas,
        &[(fake_sha, patch.to_vec())],
    )
    .expect("patch-bundle fallback lands the worker change");
    assert!(
        ws.path.join("worker.txt").exists(),
        "patch-bundle fallback applied"
    );
    assert_eq!(
        std::fs::read_to_string(ws.path.join("worker.txt")).unwrap(),
        "worker change\n",
        "patch content matches",
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn rejects_injection_in_repo_branch_or_url() {
    let dir = std::env::temp_dir().join(format!("ag-git-inj-{}", uuid::Uuid::new_v4()));
    let repos = dir.join("repos");
    let ws = dir.join("ws");
    let mut a = make_assignment("https://example.com/repo", "main");

    a.repository = "repo; rm -rf /".into();
    assert!(
        prepare_workspace(&repos, &ws, &a, &[], &[]).is_err(),
        "repo injection"
    );

    a.repository = "repo".into();
    a.default_branch = "main; touch /tmp/pwn".into();
    assert!(
        prepare_workspace(&repos, &ws, &a, &[], &[]).is_err(),
        "branch injection"
    );

    a.default_branch = "../escape".into();
    assert!(
        prepare_workspace(&repos, &ws, &a, &[], &[]).is_err(),
        "branch traversal"
    );

    a.default_branch = "main".into();
    a.git_url = "$(curl evil)".into();
    assert!(
        prepare_workspace(&repos, &ws, &a, &[], &[]).is_err(),
        "url injection"
    );

    a.git_url = "ext::sh -c evil".into();
    assert!(
        prepare_workspace(&repos, &ws, &a, &[], &[]).is_err(),
        "ext:: transport executes arbitrary commands"
    );

    a.git_url = "fd::0".into();
    assert!(
        prepare_workspace(&repos, &ws, &a, &[], &[]).is_err(),
        "fd:: transport rejected"
    );

    a.git_url = "https://example.com/repo with space".into();
    assert!(
        prepare_workspace(&repos, &ws, &a, &[], &[]).is_err(),
        "whitespace in url rejected"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Hardening P1 item 33: cleanup_workspace refuses traversal (`..`) and
/// symlinked workspace targets so a corrupt ws_path cannot delete an
/// arbitrary directory, and prune_stale_workspaces only removes direct
/// children of workspace_root.
#[test]
fn cleanup_workspace_refuses_traversal_and_symlink() {
    let root = std::env::temp_dir().join(format!("ag-ws-safe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let target = root.join("victim");
    std::fs::create_dir_all(&target).unwrap();
    // `..`-laden path: refused, victim stays.
    let escape = root.join("..").join("evil");
    cleanup_workspace(&escape, None, None);
    assert!(
        target.exists(),
        "traversal path did not delete the real dir"
    );
    // Symlink leaf pointing at an outside dir: refused.
    let outside = std::env::temp_dir().join(format!("ag-ws-out-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&outside).unwrap();
    let link = root.join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    cleanup_workspace(&link, None, None);
    assert!(
        outside.exists(),
        "symlink target was not deleted via the link"
    );
    // prune_stale_workspaces: a symlinked entry under workspace_root is
    // skipped by the is_dir() filter (DirEntry::metadata does not follow
    // the symlink), so the target is left intact. Quarantine of such
    // entries is covered by quarantine_stale_workspace_moves_unsafe_entry.
    #[cfg(unix)]
    {
        prune_stale_workspaces(&root, &root, std::time::Duration::from_secs(0));
        assert!(outside.exists(), "prune did not follow the symlink");
    }
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&outside).ok();
}

/// Hardening P1 item 33: an entry `safe_workspace_target_under` rejects
/// (here a symlink leaf pointing outside the root) is moved into
/// <root>/.quarantine/ instead of being rm-rf'd or left alone. The entry
/// may never reach the prune branch via `read_dir` (`DirEntry::metadata`
/// does not follow symlinks, so a symlink is not `is_dir()`), so the
/// quarantine helper is exercised directly — that's the actual unit under
/// test for the quarantine decision.
#[cfg(unix)]
#[test]
fn quarantine_stale_workspace_moves_unsafe_entry() {
    use std::os::unix::fs::symlink;
    let root = std::env::temp_dir().join(format!("ag-ws-quar-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let outside = std::env::temp_dir().join(format!("ag-ws-out2-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("flag"), "x").unwrap();
    let link = root.join("escapes");
    symlink(&outside, &link).unwrap();
    quarantine_stale_workspace(&link, &root);
    assert!(outside.join("flag").exists(), "outside target preserved");
    assert!(!link.exists(), "unsafe entry removed from workspace_root");
    let qdir = root.join(".quarantine");
    assert!(qdir.exists(), "quarantine dir present");
    let moved = std::fs::read_dir(&qdir)
        .unwrap()
        .flatten()
        .next()
        .expect("exactly one quarantined entry");
    assert!(
        moved.file_name().to_string_lossy().starts_with("escapes-"),
        "quarantined entry named from original"
    );
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&outside).ok();
}

/// Hardening P1 item 32 / §32 correctness: an explicitly-pinned base
/// commit that cannot be fetched and is not present locally must fail
/// closed (prepare_workspace errors) unless the operator opts into the
/// relaxed policy with AGENTGRID_ALLOW_MISSING_UPSTREAM=1.
#[test]
fn prepare_workspace_fail_closed_on_missing_pinned_base() {
    let dir = std::env::temp_dir().join(format!("ag-fc-base-{}", uuid::Uuid::new_v4()));
    let origin = dir.join("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]).unwrap();
    std::fs::write(origin.join("base.txt"), "base").unwrap();
    git(&origin, &["add", "-A"]).unwrap();
    git(
        &origin,
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
    )
    .unwrap();
    let repos = dir.join("repos");
    let ws_root = dir.join("ws");
    let url = origin.to_str().unwrap().to_string();
    let mut a = make_assignment(&url, "main");
    a.base_commit = Some("deadcafedeadcafe".into()); // not an ancestor/present

    // Default policy: fail closed.
    std::env::remove_var("AGENTGRID_ALLOW_MISSING_UPSTREAM");
    let res = prepare_workspace(&repos, &ws_root.join("a1"), &a, &[], &[]);
    let err = match res {
        Err(e) => e,
        Ok(_) => panic!("missing pinned base must fail closed by default"),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("could not be fetched") || msg.contains("not present locally"),
        "error must name the missing base: {msg}"
    );

    // Relaxed policy: falls back to default branch and returns Ok.
    std::env::set_var("AGENTGRID_ALLOW_MISSING_UPSTREAM", "1");
    let ws = prepare_workspace(&repos, &ws_root.join("a2"), &a, &[], &[]).unwrap();
    assert!(
        ws.base_commit.is_none(),
        "relaxed policy falls back to the default branch (base_commit=None)"
    );
    std::env::remove_var("AGENTGRID_ALLOW_MISSING_UPSTREAM");
    std::fs::remove_dir_all(&dir).ok();
}

/// Hardening plan 928: two daemon processes (or two attempts) on one repo
/// root must not race the bare-mirror fetch/worktree-add. flock is per
/// open-file-description, so a second open in the same process contends
/// exactly like a sibling daemon's open — this test proves the
/// serialization: holder blocks contender, release unblocks it.
#[test]
fn cross_process_flock_serializes_two_holders() {
    let dir = std::env::temp_dir().join(format!("ag-flock-928-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let root = dir.as_path();
    let repo = "repo-928";

    let first = RepoFlock::acquire(root, repo, std::time::Duration::from_secs(1)).unwrap();
    // A second acquisition (simulating the sibling daemon) must block:
    // with a 300ms timeout it must NOT acquire while the first holds.
    let started = std::time::Instant::now();
    let contended = RepoFlock::acquire(root, repo, std::time::Duration::from_millis(300));
    let waited = started.elapsed();
    assert!(
        contended.is_err(),
        "contender must time out while the first holder holds the flock"
    );
    assert!(
        waited >= std::time::Duration::from_millis(280),
        "contender must actually wait on the flock, not fail fast (waited {waited:?})"
    );

    // Release the holder: the contender now acquires immediately.
    drop(first);
    let acquired = RepoFlock::acquire(root, repo, std::time::Duration::from_secs(1));
    assert!(
        acquired.is_ok(),
        "flock is released by Drop; kernel auto-release"
    );
    drop(acquired.unwrap());
    std::fs::remove_dir_all(&dir).ok();
}

/// Plan 1.2 (#11): the deterministic pre-merge-resolve pass resolves a
/// both-add conflict (both branches appended a different import line) and
/// reports success; without the script configured it reports false (old
/// LLM-only path).
#[test]
fn resolve_trivial_conflicts_resolves_both_add() {
    let dir = std::env::temp_dir().join(format!("ag-pmr-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("f.txt"), "line1\n").unwrap();
    git(&dir, &["init", "-q", "-b", "main"]).unwrap();
    git(
        &dir,
        &["-c", "user.name=t", "-c", "user.email=t@x", "add", "-A"],
    )
    .unwrap();
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
            "base",
        ],
    )
    .unwrap();
    // Branch A appends import A.
    git(&dir, &["checkout", "-q", "-b", "feat"]).unwrap();
    std::fs::write(dir.join("f.txt"), "line1\nimport A\n").unwrap();
    git(&dir, &["add", "-A"]).unwrap();
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
            "feat",
        ],
    )
    .unwrap();
    // Branch B appends import B.
    git(&dir, &["checkout", "-q", "main"]).unwrap();
    std::fs::write(dir.join("f.txt"), "line1\nimport B\n").unwrap();
    git(&dir, &["add", "-A"]).unwrap();
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
            "main",
        ],
    )
    .unwrap();
    // Merge leaves a conflict. Identity flags required: git ≥2.5x checks
    // the committer identity before even attempting the merge, and the
    // CI runner has no global git config — without them the merge dies
    // with "Committer identity unknown" and the fixture never conflicts.
    git(
        &dir,
        &["-c", "user.name=t", "-c", "user.email=t@x", "merge", "feat"],
    )
    .ok();
    let conflicted = git_out(&dir, &["diff", "--name-only", "--diff-filter=U"]).unwrap();
    assert!(!conflicted.trim().is_empty(), "fixture must be conflicted");

    // No script configured -> built-in patterns run, but this fixture
    // (both sides add DIFFERENT lines) is non-trivial -> still false.
    std::env::remove_var("AGENTGRID_PRE_MERGE_RESOLVE");
    assert!(!resolve_trivial_conflicts(dir.to_str().unwrap()).unwrap());

    // Script configured → both-add resolved.
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("deploy/pre-merge-resolve.sh");
    std::env::set_var("AGENTGRID_PRE_MERGE_RESOLVE", &script);
    assert!(resolve_trivial_conflicts(dir.to_str().unwrap()).unwrap());
    let out = git_out(&dir, &["diff", "--name-only", "--diff-filter=U"]).unwrap();
    assert!(out.trim().is_empty(), "no conflicts must remain");
    let content = std::fs::read_to_string(dir.join("f.txt")).unwrap();
    assert!(content.contains("import A") && content.contains("import B"));

    std::env::remove_var("AGENTGRID_PRE_MERGE_RESOLVE");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn resolve_conflict_markers_trivial_cases() {
    // Identical both-add -> one copy kept.
    let c = "head\n<<<<<<< ours\nimport A\n=======\nimport A\n>>>>>>> theirs\ntail\n";
    assert_eq!(
        resolve_conflict_markers(c).unwrap(),
        "head\nimport A\ntail\n"
    );
    // One side empty -> keep the other.
    let c = "<<<<<<< ours\n=======\nnew line\n>>>>>>> theirs\n";
    assert_eq!(resolve_conflict_markers(c).unwrap(), "new line\n");
    let c = "<<<<<<< ours\nkept\n=======\n>>>>>>> theirs\n";
    assert_eq!(resolve_conflict_markers(c).unwrap(), "kept\n");
    // Whitespace-only difference -> incoming side wins.
    let c = "<<<<<<< ours\nfoo   \n\n=======\nfoo\n>>>>>>> theirs\n";
    assert_eq!(resolve_conflict_markers(c).unwrap(), "foo\n");
    // diff3 base section tolerated.
    let c = "<<<<<<< ours\nA\n||||||| base\nB\n=======\nA\n>>>>>>> theirs\n";
    assert_eq!(resolve_conflict_markers(c).unwrap(), "A\n");
}

#[test]
fn resolve_conflict_markers_non_trivial_and_malformed() {
    // Different content -> None.
    let c = "<<<<<<< ours\nA\n=======\nB\n>>>>>>> theirs\n";
    assert!(resolve_conflict_markers(c).is_none());
    // Unclosed hunk -> None.
    let c = "<<<<<<< ours\nA\n=======\nA\n";
    assert!(resolve_conflict_markers(c).is_none());
    // No markers -> None.
    assert!(resolve_conflict_markers("plain file\n").is_none());
}

#[test]
fn builtin_resolve_handles_whitespace_conflict_without_script() {
    let dir = std::env::temp_dir().join(format!("ag-pmr-bi-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("f.txt"), "line1\nfoo\n").unwrap();
    let gitc = |args: &[&str]| {
        let mut full = vec!["-c", "user.name=t", "-c", "user.email=t@x"];
        full.extend_from_slice(args);
        git(&dir, &full).unwrap()
    };
    git(&dir, &["init", "-q", "-b", "main"]).unwrap();
    gitc(&["add", "-A"]);
    gitc(&["commit", "-q", "-m", "base"]);
    // Both branches touch the same line with different trailing
    // whitespace -> a real conflict that is whitespace-only equivalent.
    git(&dir, &["checkout", "-q", "-b", "feat"]).unwrap();
    std::fs::write(dir.join("f.txt"), "line1\nfoo\t\n").unwrap();
    gitc(&["add", "-A"]);
    gitc(&["commit", "-q", "-m", "feat"]);
    git(&dir, &["checkout", "-q", "main"]).unwrap();
    std::fs::write(dir.join("f.txt"), "line1\nfoo \n").unwrap();
    gitc(&["add", "-A"]);
    gitc(&["commit", "-q", "-m", "main"]);
    // Merge conflicts (exit 1) — allow the failure.
    let _ = git(
        &dir,
        &["-c", "user.name=t", "-c", "user.email=t@x", "merge", "feat"],
    );
    let conflicted = git_out(&dir, &["diff", "--name-only", "--diff-filter=U"]).unwrap();
    assert!(!conflicted.trim().is_empty(), "fixture must be conflicted");

    std::env::remove_var("AGENTGRID_PRE_MERGE_RESOLVE");
    assert!(
        resolve_trivial_conflicts(dir.to_str().unwrap()).unwrap(),
        "whitespace-only conflict must resolve via built-in patterns"
    );
    let out = git_out(&dir, &["diff", "--name-only", "--diff-filter=U"]).unwrap();
    assert!(out.trim().is_empty(), "no conflicts must remain");
    let content = std::fs::read_to_string(dir.join("f.txt")).unwrap();
    assert_eq!(content, "line1\nfoo\t\n", "incoming side wins");

    std::fs::remove_dir_all(&dir).ok();
}
