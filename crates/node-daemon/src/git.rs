//! Git worktree preparation/finalization for an attempt (Stage 2.5).
//!
//! Git-backed tasks keep one clone per (node, repository) under
//! `repository_root/<name>`; each attempt gets a dedicated worktree on a
//! branch `agent/<task-id>/<n>`. Plain-dir tasks (empty `git_url`) just get a
//! fresh directory and no commit.
//!
//! Every git invocation passes one argument per token through `Command::arg`
//! (no `sh -c`), so a crafted `git_url`/`repository`/`branch` from the control
//! plane cannot inject a shell command. Tokens are validated as defense-in-depth
//! (Stage 2.3).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use agentgrid_common::Assignment;
use anyhow::{Context, Result};

/// Per-repo in-process lock (Stage 2.3): the shared clone's `fetch` +
/// `checkout -B` + `worktree add` are serialized per repository so two
/// parallel attempts of one repo cannot race the clone state. Each attempt
/// still gets its own worktree, so agent work runs concurrently.
static REPO_LOCKS: OnceLock<Mutex<HashMap<String, std::sync::Arc<Mutex<()>>>>> = OnceLock::new();

fn repo_lock(repo: &str) -> std::sync::Arc<Mutex<()>> {
    let map = REPO_LOCKS.get_or_init(Mutex::default);
    let mut guard = map.lock().unwrap();
    guard
        .entry(repo.to_string())
        .or_insert_with(|| std::sync::Arc::new(Mutex::new(())))
        .clone()
}

pub struct Workspace {
    /// Directory the adapter runs in.
    pub path: PathBuf,
    /// Local clone dir (None for plain-dir tasks).
    pub repo_dir: Option<PathBuf>,
    /// Attempt branch (None for plain-dir tasks).
    pub branch: Option<String>,
    pub default_branch: String,
    pub is_git: bool,
    /// Optional exact commit the worktree was pinned to (Stage 8 base_commit).
    pub base_commit: Option<String>,
}

/// Run `git` with explicit args (no shell). Args are passed verbatim, so they
/// cannot be reinterpreted as shell syntax.
fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .context("failed to spawn git")?;
    if !status.success() {
        anyhow::bail!("git {:?} failed", args);
    }
    Ok(())
}

/// Like [`git`] but capture stdout (trimmed).
fn git_out(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .context("failed to spawn git")?;
    if !out.status.success() {
        anyhow::bail!("git {:?} failed", args);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Per-worktree path to git's `info/exclude`, resolved via `git rev-parse`
/// so linked worktrees get their own gitdir-scoped file, not the shared clone's.
fn worktree_git_info_exclude(ws: &Path) -> Option<PathBuf> {
    match git_out(ws, &["rev-parse", "--git-path", "info/exclude"]) {
        Ok(s) if !s.is_empty() => {
            let p = PathBuf::from(&s);
            if p.is_absolute() {
                Some(p)
            } else {
                Some(ws.join(p))
            }
        }
        _ => Some(ws.join(".git").join("info").join("exclude")),
    }
}

/// Reject git ref / slug tokens that could enable traversal or shell injection.
/// Git is invoked without a shell, so this is defense-in-depth against malformed
/// control-plane input (Stage 2.3).
fn validate_token(s: &str) -> Result<()> {
    if s.is_empty()
        || s.chars().any(|c| "\"';|&$()`><\\\n\t{}".contains(c))
        || s.contains("..")
        || s.starts_with('/')
    {
        anyhow::bail!("unsafe git token: {s:?}");
    }
    Ok(())
}

/// Reject a git URL that embeds shell metacharacters (defense-in-depth; the URL
/// is passed as a single git argument, not through a shell).
fn validate_git_url(s: &str) -> Result<()> {
    if s.chars().any(|c| "\"';|&$()`><\\\n\t".contains(c)) {
        anyhow::bail!("unsafe git url: {s:?}");
    }
    Ok(())
}

/// Ensure the repo clone exists and create a per-attempt worktree.
pub fn prepare_workspace(
    repository_root: &Path,
    workspace_root: &Path,
    assignment: &Assignment,
) -> Result<Workspace> {
    let ws = workspace_root.join(&assignment.attempt_id);
    std::fs::create_dir_all(&ws)?;
    if assignment.git_url.is_empty() {
        return Ok(Workspace {
            path: ws,
            repo_dir: None,
            branch: None,
            default_branch: String::new(),
            is_git: false,
            base_commit: None,
        });
    }
    validate_token(&assignment.repository)?;
    validate_token(&assignment.task_id)?;
    validate_token(&assignment.default_branch)?;
    validate_git_url(&assignment.git_url)?;

    let repo_dir = repository_root.join(&assignment.repository);
    let branch = format!("agent/{}/{}", assignment.task_id, assignment.number);
    let db = assignment.default_branch.as_str();
    let gurl = assignment.git_url.as_str();
    let repo = assignment.repository.as_str();

    // Stage 2.3: serialize shared-clone mutations (fetch / checkout -B /
    // worktree add) per repository across concurrent attempts.
    let _repo_arc = repo_lock(repo);
    let _repo_guard = _repo_arc.lock().unwrap();

    // Stage 8: if a fixed base_commit is requested, every attempt of this step
    // starts from that exact commit (parallel workers share it). Best-effort
    // fetch so the commit is present locally, then validate the token
    // (defense-in-depth: git is invoked without a shell).
    let base_commit = assignment
        .base_commit
        .as_ref()
        .filter(|c| !c.is_empty())
        .map(|c| {
            validate_token(c)?;
            let _ = Command::new("git")
                .args(["fetch", "origin", c])
                .current_dir(&repo_dir)
                .status();
            Ok::<&str, anyhow::Error>(c.as_str())
        })
        .transpose()?;

    if repo_dir.join(".git").exists() {
        git(&repo_dir, &["fetch", "origin", db])?;
    } else {
        std::fs::create_dir_all(repository_root)?;
        git(repository_root, &["clone", gurl, repo])?;
    }
    git(&repo_dir, &["checkout", "-B", db, &format!("origin/{db}")])?;
    let start_point = base_commit.unwrap_or(db);
    git(
        &repo_dir,
        &[
            "worktree",
            "add",
            ws.to_str().unwrap_or(""),
            "-b",
            &branch,
            start_point,
        ],
    )?;
    // Stage 2.2: keep agent-side logs and our own patch out of the commit / diff.
    // `.git/info/exclude` is per-worktree gitdir for linked worktrees, so this
    // scopes to this attempt only and does not touch the shared clone.
    let exclude = worktree_git_info_exclude(&ws);
    if let Some(p) = exclude {
        let mut cur = std::fs::read_to_string(&p).unwrap_or_default();
        for name in ["agent-raw-output.log", "validation.log", "changes.patch"] {
            if !cur.contains(name) {
                cur.push_str(&format!("{name}\n"));
            }
        }
        std::fs::write(&p, cur)?;
    }
    Ok(Workspace {
        path: ws,
        repo_dir: Some(repo_dir),
        branch: Some(branch),
        default_branch: assignment.default_branch.clone(),
        is_git: true,
        base_commit: base_commit.map(|c| c.to_string()),
    })
}

/// Commit any staged changes and write a binary diff (`changes.patch`) into the
/// workspace. Returns the commit SHA (or current HEAD for no-op), None for
/// plain-dir tasks.
pub fn finalize_workspace(ws: Workspace, committer_email: &str) -> Result<Option<String>> {
    let (repo_dir, branch) = match (&ws.repo_dir, &ws.branch) {
        (Some(r), Some(b)) => (r, b),
        _ => return Ok(None),
    };
    git(&ws.path, &["add", "-A"])?;
    let has_changes = !Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(&ws.path)
        .status()?
        .success();
    let sha = if has_changes {
        git(
            &ws.path,
            &[
                "-c",
                "user.name=agentgrid",
                "-c",
                &format!("user.email={committer_email}"),
                "commit",
                "-m",
                &format!("agentgrid: {branch}"),
            ],
        )?;
        git_out(&ws.path, &["rev-parse", "HEAD"])?
    } else {
        git_out(&ws.path, &["rev-parse", "HEAD"])?
    };
    let diff_base = ws.base_commit.clone().unwrap_or(ws.default_branch.clone());
    let patch = git_out(repo_dir, &["diff", &diff_base, branch, "--binary"])?;
    std::fs::write(ws.path.join("changes.patch"), patch)?;
    Ok(Some(sha))
}

/// Remove the per-attempt worktree dir and (for git tasks) its branch after
/// the attempt is done (Stage 2.3 worktree/branch cleanup). Best-effort: logs
/// and swallows errors so a stuck worktree never turns a successful attempt
/// terminal. For git tasks `git worktree remove --force` drops the worktree
/// dir and its gitlink, and the branch delete (best-effort) reclaims the ref.
/// The worktree dir is removed directly as a fallback if `worktree remove`
/// left it behind — and as the only step for non-git tasks (plain dir).
pub fn cleanup_workspace(
    ws_path: &std::path::Path,
    repo_dir: Option<&std::path::Path>,
    branch: Option<&str>,
) {
    if let (Some(repo), Some(branch)) = (repo_dir, branch) {
        if let Err(e) = (|| -> Result<()> {
            git(
                repo,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    ws_path.to_str().unwrap_or(""),
                ],
            )?;
            let _ = Command::new("git")
                .args(["branch", "-D", branch])
                .current_dir(repo)
                .status();
            Ok(())
        })() {
            tracing::warn!(?ws_path, "worktree remove failed: {e}; falling back to rm");
        }
    }
    if ws_path.exists() {
        let _ = std::fs::remove_dir_all(ws_path);
    }
}

/// Reclaim per-attempt workspace dirs and worktree gitlinks left by a prior
/// daemon run that was killed before its graceful `cleanup_workspace` ran. A
/// dir is removed only if its mtime is older than `retention` (so an in-flight
/// attempt on a just-restarted node isn't swept). For each repo under
/// `repository_root`, also runs `git worktree prune` to drop gitlinks whose
/// worktrees no longer exist. Best-effort.
pub fn prune_stale_workspaces(
    workspace_root: &std::path::Path,
    repository_root: &std::path::Path,
    retention: std::time::Duration,
) {
    let cutoff = std::time::SystemTime::now() - retention;
    if let Ok(entries) = std::fs::read_dir(workspace_root) {
        for e in entries.flatten() {
            if let Ok(md) = e.metadata() {
                if md.is_dir() {
                    if let Ok(mtime) = md.modified() {
                        if mtime < cutoff {
                            let p = e.path();
                            tracing::info!(?p, "pruning stale workspace dir");
                            let _ = std::fs::remove_dir_all(&p);
                        }
                    }
                }
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(repository_root) {
        for e in entries.flatten() {
            if e.path().join(".git").exists() {
                let _ = git(&e.path(), &["worktree", "prune"]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentgrid_common::Assignment;

    fn make_assignment(git_url: &str, default_branch: &str) -> Assignment {
        Assignment {
            attempt_id: "attempt-test".into(),
            task_id: "task-test".into(),
            repository: "repo".into(),
            prompt: "x".into(),
            adapter: "mock".into(),
            number: 1,
            timeout_secs: 60,
            git_url: git_url.into(),
            default_branch: default_branch.into(),
            validation_command: None,
            base_commit: None,
            parent_acp_session_id: None,
        }
    }

    #[test]
    fn plain_dir_has_no_commit() {
        let dir = std::env::temp_dir().join(format!("ag-git-plain-{}", uuid::Uuid::new_v4()));
        let ws_root = dir.join("ws");
        let a = make_assignment("", "main");
        let ws = prepare_workspace(&dir.join("repos"), &ws_root, &a).unwrap();
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
        let ws = prepare_workspace(&dir.join("repos"), &dir.join("ws"), &a).unwrap();
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
        let ws = prepare_workspace(&dir.join("repos"), &dir.join("ws"), &a).unwrap();
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
        let dir =
            std::env::temp_dir().join(format!("ag-git-cleanup-plain-{}", uuid::Uuid::new_v4()));
        let a = make_assignment("", "main");
        let ws = prepare_workspace(&dir.join("repos"), &dir.join("ws"), &a).unwrap();
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
        let ws = prepare_workspace(&dir.join("repos"), &dir.join("ws"), &a).unwrap();
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
        let ws = prepare_workspace(&dir.join("repos"), &dir.join("ws"), &a).unwrap();
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
                    task_id: format!("task-{n}"),
                    repository: "repo".into(),
                    prompt: "x".into(),
                    adapter: "mock".into(),
                    number: 1,
                    timeout_secs: 60,
                    git_url: url,
                    default_branch: "main".into(),
                    validation_command: None,
                    base_commit: None,
                    parent_acp_session_id: None,
                };
                prepare_workspace(&repos, &ws_root, &a)
            }));
        }
        let mut ok = 0;
        for h in handles {
            if let Ok(ws) = h.join().unwrap() {
                assert!(ws.is_git, "worktree should be a git worktree");
                assert!(ws.path.exists(), "worktree path must exist");
                ok += 1;
            }
        }
        assert_eq!(ok, 4, "all parallel prepares must succeed");
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
            prepare_workspace(&repos, &ws, &a).is_err(),
            "repo injection"
        );

        a.repository = "repo".into();
        a.default_branch = "main; touch /tmp/pwn".into();
        assert!(
            prepare_workspace(&repos, &ws, &a).is_err(),
            "branch injection"
        );

        a.default_branch = "../escape".into();
        assert!(
            prepare_workspace(&repos, &ws, &a).is_err(),
            "branch traversal"
        );

        a.default_branch = "main".into();
        a.git_url = "$(curl evil)".into();
        assert!(prepare_workspace(&repos, &ws, &a).is_err(), "url injection");
        std::fs::remove_dir_all(&dir).ok();
    }
}
