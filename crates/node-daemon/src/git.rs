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

use rand::Rng;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use agentgrid_common::Assignment;
use anyhow::{Context, Result};

/// Per-repo in-process lock (Stage 2.3): the bare-mirror clone's `fetch` +
/// `worktree add` are serialized per repository so two parallel attempts of one
/// repo cannot race the clone state (no more `checkout -B` on a shared
/// clone). Each attempt
/// still gets its own worktree, so agent work runs concurrently.
static REPO_LOCKS: OnceLock<Mutex<HashMap<String, std::sync::Arc<Mutex<()>>>>> = OnceLock::new();

/// Hardening P2 item 35: cumulative milliseconds spent waiting on the
/// per-repo lock (in-process mutex + cross-process flock). Reported via the
/// heartbeat so the control plane can surface repository contention.
static REPO_LOCK_WAIT_MS: AtomicU64 = AtomicU64::new(0);

/// Cumulative repository-lock wait in milliseconds (see [`REPO_LOCK_WAIT_MS`]).
pub fn repo_lock_wait_ms() -> u64 {
    REPO_LOCK_WAIT_MS.load(Ordering::Relaxed)
}

/// Hardening P2 item 35: repository cache size in bytes.
/// Updated on clone/fetch with periodic disk size refresh.
static REPO_CACHE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Hardening P2 item 35: workspace size in bytes.
/// Updated on worktree add with periodic disk size refresh.
static WORKSPACE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Returns the current repository cache size in bytes.
pub fn repo_cache_bytes() -> u64 {
    REPO_CACHE_BYTES.load(Ordering::Relaxed)
}

/// Returns the current workspace size in bytes.
pub fn workspace_bytes() -> u64 {
    WORKSPACE_BYTES.load(Ordering::Relaxed)
}

fn repo_lock(repo: &str) -> std::sync::Arc<Mutex<()>> {
    let map = REPO_LOCKS.get_or_init(Mutex::default);
    let mut guard = map.lock().unwrap();
    guard
        .entry(repo.to_string())
        .or_insert_with(|| std::sync::Arc::new(Mutex::new(())))
        .clone()
}

/// Hardening P2 item 35: get configured quota limits from environment.
/// Returns None if unlimited (env not set or 0).
fn repo_cache_quota_mb() -> Option<u64> {
    std::env::var("AGENTGRID_REPO_CACHE_QUOTA_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
}

fn workspace_quota_mb() -> Option<u64> {
    std::env::var("AGENTGRID_WORKSPACE_QUOTA_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
}

/// Hardening P2 item 35: check if adding `additional_bytes` would exceed the
/// repository cache quota. Returns Err if quota would be exceeded.
fn check_repo_cache_quota(additional_bytes: u64) -> Result<()> {
    if let Some(quota_mb) = repo_cache_quota_mb() {
        let current = REPO_CACHE_BYTES.load(Ordering::Relaxed);
        let new_total = current.saturating_add(additional_bytes);
        if new_total > quota_mb * 1024 * 1024 {
            anyhow::bail!(
                "repository cache quota exceeded: current {} MB + {} MB > {} MB limit",
                current / 1024 / 1024,
                additional_bytes / 1024 / 1024,
                quota_mb
            );
        }
    }
    Ok(())
}

/// Hardening P2 item 35: check if adding `additional_bytes` would exceed the
/// workspace quota. Returns Err if quota would be exceeded.
fn check_workspace_quota(additional_bytes: u64) -> Result<()> {
    if let Some(quota_mb) = workspace_quota_mb() {
        let current = WORKSPACE_BYTES.load(Ordering::Relaxed);
        let new_total = current.saturating_add(additional_bytes);
        if new_total > quota_mb * 1024 * 1024 {
            anyhow::bail!(
                "workspace quota exceeded: current {} MB + {} MB > {} MB limit",
                current / 1024 / 1024,
                additional_bytes / 1024 / 1024,
                quota_mb
            );
        }
    }
    Ok(())
}

/// Hardening P2 item 35: periodically refresh quota metrics from actual disk
/// usage to account for external changes (e.g., manual cleanup, pruning).
/// Called with ~10% probability per check to avoid excessive stat calls.
fn update_quota_metrics(repository_root: &Path, workspace_root: &Path) {
    use std::sync::atomic::Ordering;
    // 10% chance to refresh from disk
    if rand::thread_rng().gen_range(0..=9) == 0 {
        if let Ok(size) = dir_size(repository_root) {
            REPO_CACHE_BYTES.store(size, Ordering::Relaxed);
        }
        if let Ok(size) = dir_size(workspace_root) {
            WORKSPACE_BYTES.store(size, Ordering::Relaxed);
        }
    }
}

/// Calculate total size of a directory in bytes.
fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    if path.exists() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            } else if metadata.is_dir() {
                total = total.saturating_add(dir_size(&entry.path())?);
            }
        }
    }
    Ok(total)
}

/// Hardening P1 item 32: a cross-process `flock` on a per-repo lock file so two
/// node-daemon processes (or a restarted daemon) cannot race the bare-mirror
/// clone's `fetch`/`worktree add`. Held alongside the in-process `repo_lock`.
/// Blocks up to `timeout`; returns Err on timeout so the attempt fails loudly
/// rather than wedging.
struct RepoFlock {
    file: std::fs::File,
}

impl RepoFlock {
    fn acquire(repository_root: &Path, repo: &str, timeout: std::time::Duration) -> Result<Self> {
        std::fs::create_dir_all(repository_root).ok();
        let lock_path = repository_root.join(format!("{repo}.lock"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open repo lock {lock_path:?}"))?;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            // libc::flock: exclusive, non-blocking; retry on contention.
            let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(RepoFlock { file });
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::WouldBlock {
                return Err(err).context("flock repo lock");
            }
            if std::time::Instant::now() >= deadline {
                // Hardening P1 item 32: diagnostics + recovery note. A
                // `flock(LOCK_EX)` is released by the kernel when the holder
                // dies, so a genuine stale lock is rare — a timeout here almost
                // always means a long-running clone/fetch by a sibling process.
                // Surface the repo + the lock file path so an operator can check
                // the holder. The kernel auto-releases on holder exit, so no
                // manual stale-lock deletion is needed.
                tracing::warn!(
                    repo = %repo,
                    lock = %lock_path.display(),
                    "timed out waiting for cross-process repo lock — a sibling clone/fetch may be in progress; kernel releases flock on holder exit"
                );
                anyhow::bail!("timed out waiting for repo lock on {repo}");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

impl Drop for RepoFlock {
    fn drop(&mut self) {
        let fd = std::os::fd::AsRawFd::as_raw_fd(&self.file);
        unsafe { libc::flock(fd, libc::LOCK_UN) };
    }
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

/// Hardening P1 item 32: true if `maybe_ancestor` is an ancestor of (or equal
/// to) `descendant`. Uses `git merge-base --is-ancestor`; best-effort — returns
/// false on any git error so the call site falls back to "not stale".
fn is_ancestor_or_equal(repo: &Path, maybe_ancestor: &str, descendant: &str) -> bool {
    let out = Command::new("git")
        .args(["merge-base", "--is-ancestor", maybe_ancestor, descendant])
        .current_dir(repo)
        .output();
    match out {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

/// Like [`git_out`] but returns raw bytes — required for binary diffs (`git
/// diff --binary`), where `String::from_utf8_lossy` would corrupt non-UTF-8
/// hunk data. (Hardening P1 item 32: preserve binary patch bytes.)
fn git_out_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .context("failed to spawn git")?;
    if !out.status.success() {
        anyhow::bail!("git {:?} failed", args);
    }
    let mut v = out.stdout;
    // Mirror git_out's trim of a single trailing newline.
    if v.last() == Some(&b'\n') {
        v.pop();
    }
    Ok(v)
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
/// `upstream_patches` carries `changes.patch` bytes fetched from the control
/// plane for SHAs that may not be reachable via the shared Git remote — a
/// fallback for distributed workflows without a shared remote (Stage 8 /
/// line 257). When a cherry-pick SHA is missing from the local mirror, the
/// matching patch (keyed by SHA) is applied instead; if neither the SHA nor a
/// patch is available the upstream is skipped (best-effort, the integrator
/// still runs with one fewer merged worker).
pub fn prepare_workspace(
    repository_root: &Path,
    workspace_root: &Path,
    assignment: &Assignment,
    upstream_commits: &[String],
    upstream_patches: &[(String, Vec<u8>)],
) -> Result<Workspace> {
    // Hardening P2 item 35: periodically refresh quota metrics from disk.
    update_quota_metrics(repository_root, workspace_root);

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

    // Stage 2.3: serialize shared bare-mirror mutations (fetch / worktree
    // add) per repository across concurrent attempts.
    let _repo_arc = repo_lock(repo);
    // Hardening P2 item 35: account block time waiting on the per-repo lock
    // so an operator can alert on repository contention.
    let lock_started = std::time::Instant::now();
    let _repo_guard = _repo_arc.lock().unwrap();
    let _flock = RepoFlock::acquire(repository_root, repo, std::time::Duration::from_secs(60))?;
    REPO_LOCK_WAIT_MS.fetch_add(lock_started.elapsed().as_millis() as u64, Ordering::Relaxed);

    // Stage 2.3 (bare mirror): keep a single bare `--mirror` clone per repo so
    // the shared clone has no working tree and no HEAD to mutate — attempts no
    // longer `checkout -B` a branch into the shared clone (which flapped HEAD
    // between parallel attempts using different default branches/commits).
    // All refs are mirrored under the same names (`refs/heads/main` etc.), so
    // the default branch is addressed by `db` directly (no `origin/` prefix).
    if repo_dir.join("HEAD").exists() {
        // Already a bare mirror: refresh all refs.
        git(&repo_dir, &["fetch", "origin", "--prune"])?;
        // Hardening P2 item 35: check repo cache quota after fetch.
        update_quota_metrics(repository_root, workspace_root);
        if let Ok(size) = dir_size(repository_root) {
            check_repo_cache_quota(size.saturating_sub(REPO_CACHE_BYTES.load(Ordering::Relaxed)))?;
            REPO_CACHE_BYTES.store(size, Ordering::Relaxed);
        }
    } else {
        std::fs::create_dir_all(repository_root)?;
        git(repository_root, &["clone", "--mirror", gurl, repo])?;
        // Hardening P2 item 35: check repo cache quota after clone.
        if let Ok(size) = dir_size(repository_root) {
            check_repo_cache_quota(size)?;
            REPO_CACHE_BYTES.store(size, Ordering::Relaxed);
        }
    }

    // Stage 8: if a fixed base_commit is requested, every attempt of this step
    // starts from that exact commit (parallel workers share it). Best-effort
    // fetch so the commit is present locally; validate the token (defense in
    // depth).
    let base_commit = assignment
        .base_commit
        .as_ref()
        .filter(|c| !c.is_empty())
        .map(|c| {
            validate_token(c)?;
            let fetch_ok = Command::new("git")
                .args(["fetch", "origin", c])
                .current_dir(&repo_dir)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            // Hardening P1 item 32 / §32 correctness: fail CLOSED when an
            // explicitly-pinned base commit cannot be fetched, unless the
            // operator opts into the relaxed policy with
            // `AGENTGRID_ALLOW_MISSING_UPSTREAM=1` (then we fall back to the
            // default branch and warn loudly). The explicit opt-in is the
            // documented escape hatch for distributed workflows without a
            // shared remote; a silent fall-through to the default branch would
            // run the agent against the wrong base without any signal.
            let allow_missing = std::env::var("AGENTGRID_ALLOW_MISSING_UPSTREAM")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            let present_locally = Command::new("git")
                .args(["cat-file", "-e", c])
                .current_dir(&repo_dir)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !fetch_ok && !present_locally {
                if allow_missing {
                    tracing::warn!(
                        c,
                        "base_commit fetch failed and not present locally; \
                         AGENTGRID_ALLOW_MISSING_UPSTREAM=1 → falling back to {} \
                         (operator opt-in)",
                        db,
                    );
                    return Ok(None::<&str>);
                }
                anyhow::bail!(
                    "pinned base commit {c} could not be fetched and is not present \
                     locally; set AGENTGRID_ALLOW_MISSING_UPSTREAM=1 to fall back to \
                     the default branch",
                );
            }
            // Hardening P1 item 32: warn if the pinned base commit is behind
            // the remote default branch HEAD (stale base). The agent still runs
            // (operator pinned it on purpose), but the warning surfaces drift
            // so a rebase/follow-up is not forgotten.
            if let Ok(remote_head) = git_out(&repo_dir, &["rev-parse", &format!("refs/heads/{db}")]) {
                let remote_head = remote_head.trim();
                if !remote_head.is_empty() && !is_ancestor_or_equal(&repo_dir, remote_head, c) {
                    tracing::warn!(
                        "pinned base_commit {} is not an ancestor of remote {} HEAD {} — running against a stale base",
                        c,
                        db,
                        remote_head
                    );
                }
            }
            Ok(Some(c.as_str()))
        })
        .transpose()?
        // Flatten Option<Option<&str>> -> Option<&str>: a fall-back to the
        // default branch yields None (let start_point = db).
        .flatten();

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
    // Hardening P2 item 35: check workspace quota after worktree add.
    if let Ok(size) = dir_size(workspace_root) {
        check_workspace_quota(size.saturating_sub(WORKSPACE_BYTES.load(Ordering::Relaxed)))?;
        WORKSPACE_BYTES.store(size, Ordering::Relaxed);
    }
    // Stage 8 / line 239 / line 240: land upstream worker commits into
    // this worktree before the agent runs.
    //  - Integrator: cherry-pick *each* upstream worker commit so the worktree
    //    starts from the integrated worker changes (an integration branch).
    //  - Verifier: cherry-pick its (usually single) upstream worker commit so
    //    the worktree starts at the worker's tree on top of the base — letting
    //    the verifier read the worker's change for the verdict, without ever
    //    seeing the worker's private transcripts (ADR: handoffs reference
    //    commits, not logs; isolation holds because the worktree only has the
    //    commit, never the worker's logs).
    // Each upstream SHA is fetched (best-effort, may already be in the mirror)
    // and cherry-picked onto the new branch. A conflicting commit aborts the
    // cherry-pick and surfaces to the agent via a non-zero prep error; no
    // partial state is committed (the worktree stays on the clean
    // `start_point`).
    if !upstream_commits.is_empty() {
        let ws_path = ws.to_str().unwrap_or("");
        for sha in upstream_commits {
            validate_token(sha)?;
            // Best-effort: ensure the commit object is present in the local
            // mirror so cherry-pick can resolve it. Stage 8 / line 257:
            // without a shared Git remote the SHA may never arrive — fall
            // back to the worker's `changes.patch` artifact fetched from the
            // control plane and applied with `git apply --3way`.
            let _ = Command::new("git")
                .args(["fetch", "origin", sha])
                .current_dir(&repo_dir)
                .status();
            let have_obj = Command::new("git")
                .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
                .current_dir(ws_path)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if have_obj {
                let cp = Command::new("git")
                    .args([
                        "-c",
                        "user.name=agentgrid",
                        "-c",
                        "user.email=agentgrid@agentgrid",
                        "cherry-pick",
                        sha,
                    ])
                    .current_dir(ws_path)
                    .output()?;
                if !cp.status.success() {
                    let _ = Command::new("git")
                        .args(["cherry-pick", "--abort"])
                        .current_dir(ws_path)
                        .status();
                    anyhow::bail!(
                        "integrator cherry-pick of upstream commit {sha} conflicted; \
                         merged branches need manual resolution or non-conflicting workers"
                    );
                }
                continue;
            }
            // SHA not reachable via the shared remote: apply the worker's
            // binary patch artifact fetched from the control plane.
            let Some(patch) = upstream_patches.iter().find(|(s, _)| s == sha) else {
                tracing::warn!(
                    "integrator: upstream commit {sha} not reachable and no \
                     changes.patch artifact available; skipping this upstream"
                );
                continue;
            };
            let mut ap = Command::new("git")
                .args(["apply", "--3way", "--whitespace=nowarn", "-"])
                .current_dir(ws_path)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| anyhow::anyhow!("git apply spawn: {e}"))?;
            use std::io::Write;
            {
                let mut stdin = ap.stdin.take().unwrap();
                stdin.write_all(&patch.1)?;
            }
            let out = ap.wait_with_output()?;
            if !out.status.success() {
                anyhow::bail!(
                    "integrator `git apply` of upstream changes.patch ({sha}) \
                     conflicted: {}; merged branches need manual resolution or \
                     non-conflicting workers",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
    }
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
    // Hardening P1 item 32: surface submodules / Git LFS so an operator is
    // warned that the worktree pulls additional sources (submodules) or
    // out-of-band object stores (LFS) the adapter may not fetch. We only
    // warn here (not refuse): a real policy belongs in the sandbox network /
    // mount layer.
    if ws.join(".gitmodules").exists() {
        tracing::warn!(
            "worktree {:?} contains git submodules (.gitmodules) — ensure the sandbox blocks network unless intended",
            ws
        );
    }
    if ws.join(".gitattributes").exists()
        && std::fs::read_to_string(ws.join(".gitattributes"))
            .unwrap_or_default()
            .contains("filter=lfs")
    {
        tracing::warn!(
            "worktree {:?} references Git LFS objects — agent will not have the LFS blobs unless smudge is enabled",
            ws
        );
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
    // Hardening P1 item 32: capture the binary diff as raw bytes so a
    // binary patch's non-UTF-8 hunk data is not corrupted by lossy conversion.
    let patch = git_out_bytes(repo_dir, &["diff", &diff_base, branch, "--binary"])?;
    std::fs::write(ws.path.join("changes.patch"), patch)?;
    Ok(Some(sha))
}

/// Hardening P1 item 33: a workspace path is safe to `remove_dir_all` only if
/// it has no `..` component and is not itself a symlink. (No canonicalized
/// root available in `cleanup_workspace`; the `..` + symlink checks block the
/// traversal and redirect classes of attack.)
fn safe_workspace_target(p: &std::path::Path) -> bool {
    use std::path::Component;
    // Reject any ParentDir (`..`) or Windows prefix component — these are the
    // traversal vectors. Absolute paths (RootDir) and normal segments are fine.
    for c in p.components() {
        if matches!(c, Component::ParentDir | Component::Prefix(_)) {
            return false;
        }
    }
    // Reject if the leaf is a symlink (redirect outside the workspace).
    match std::fs::symlink_metadata(p) {
        Ok(md) => !md.file_type().is_symlink(),
        Err(_) => true, // does not exist yet — allow (remove is a no-op)
    }
}

/// Like `safe_workspace_target` but additionally requires `p` to be a direct
/// child of `root` (used by `prune_stale_workspaces`, which knows the root).
fn safe_workspace_target_under(p: &std::path::Path, root: &std::path::Path) -> bool {
    if !safe_workspace_target(p) {
        return false;
    }
    let parent = match p.parent() {
        Some(par) => par,
        None => return false,
    };
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let canon_parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    canon_parent == canon_root
}

/// Hardening P1 item 33: quarantine (rather than delete) a stale workspace
/// entry that `safe_workspace_target_under` rejected (symlink leaf / outside
/// the root / traversal). The entry is moved to
/// `<workspace_root>/.quarantine/<original-name>-<timestamp>` so the
/// misconfigured/stale dir is preserved for forensics instead of silently
/// surviving or being rm-rf'd outside the root. Returns early on any IO
/// error (it is best-effort cleanup, never panic-safe-blocking).
pub(crate) fn quarantine_stale_workspace(p: &std::path::Path, workspace_root: &std::path::Path) {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        tracing::warn!(?p, "quarantine: cannot read entry name; skipping");
        return;
    };
    let qdir = workspace_root.join(".quarantine");
    if let Err(e) = std::fs::create_dir_all(&qdir) {
        tracing::warn!(?p, ?qdir, error = %e, "quarantine: mkdir failed; skipping");
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dest = qdir.join(format!("{name}-{ts}"));
    match std::fs::rename(p, &dest) {
        Ok(()) => tracing::warn!(?p, ?dest, "quarantined unsafe stale workspace entry"),
        Err(e) => tracing::warn!(?p, ?dest, error = %e, "quarantine: rename failed; skipped"),
    }
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
    // Hardening P1 item 33: never remove_dir_all a path that escapes upward
    // or is a symlink — a corrupt/attacker-controlled ws_path must not let us
    // nuke an arbitrary directory. `safe_workspace_target` rejects `..`, any
    // symlink on the path, and requires a real parent dir.
    if !safe_workspace_target(ws_path) {
        tracing::warn!(?ws_path, "refusing to clean unsafe workspace path");
        return;
    }
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
///
/// Hardening P1 item 33/35: returns prune/ quarantine/worktree counts for
/// cleanup observability (logged by the caller).
#[derive(Debug, Clone, Copy, Default)]
pub struct PruneStats {
    pub pruned: u64,
    pub quarantined: u64,
    pub worktrees_pruned: u64,
}

pub fn prune_stale_workspaces(
    workspace_root: &std::path::Path,
    repository_root: &std::path::Path,
    retention: std::time::Duration,
) -> PruneStats {
    let mut stats = PruneStats::default();
    let cutoff = std::time::SystemTime::now() - retention;
    if let Ok(entries) = std::fs::read_dir(workspace_root) {
        for e in entries.flatten() {
            if let Ok(md) = e.metadata() {
                if md.is_dir() {
                    if let Ok(mtime) = md.modified() {
                        if mtime < cutoff {
                            let p = e.path();
                            tracing::info!(?p, "pruning stale workspace dir");
                            // Hardening P1 item 33: only remove a direct
                            // child of workspace_root that is not a symlink.
                            // Unsafe entries (symlink/traversal) are
                            // quarantined under <workspace_root>/.quarantine/
                            // instead of being deleted, so a misconfigured
                            // stale dir is preserved forensics instead of
                            // silently surviving / corrupting the cleanup.
                            if safe_workspace_target_under(&p, workspace_root) {
                                let _ = std::fs::remove_dir_all(&p);
                                stats.pruned += 1;
                            } else {
                                quarantine_stale_workspace(&p, workspace_root);
                                stats.quarantined += 1;
                            }
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
                // Hardening P1 item 32: repository cache GC policy — run
                // `git gc --auto` on each bare mirror so incremental pack
                // growth is compacted without ever deleting the mirror
                // (removing a mirror while an attempt uses it would break the
                // worktree). `--auto` decides itself whether a gc is needed,
                // so the cost stays near-zero on healthy repos.
                let _ = git(&e.path(), &["gc", "--auto", "--quiet"]);
                stats.worktrees_pruned += 1;
            }
        }
    }
    stats
}

/// Hardening P1 item 32: capture the remote (upstream) HEAD of the bare-mirror
/// at the current moment by querying `origin` via `ls-remote`. Used once before
/// the agent runs and once before completion, so the attempt row records what
/// upstream looked like at start and how it moved during the attempt. Returns
/// `None` on any git/network failure (audit data — must never block the attempt).
/// ponytail: a full `git fetch` is not needed; `ls-remote` hits `origin` (the
/// configured remote URL) directly and reads only the tip.
pub fn remote_head_at(repo_dir: &Path) -> Option<String> {
    let out = git_out(repo_dir, &["ls-remote", "origin", "HEAD"]).ok()?;
    let sha = out.split_whitespace().next()?;
    // Hardening P1 item 32: accept only a plausible hex git SHA (>= 7 hex
    // chars); any other output (e.g. a git error line) yields None.
    let valid = sha.len() >= 7 && sha.bytes().all(|b| b.is_ascii_hexdigit());
    valid.then_some(sha.to_string())
}

#[cfg(test)]
mod tests {
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
        let dir =
            std::env::temp_dir().join(format!("ag-git-cleanup-plain-{}", uuid::Uuid::new_v4()));
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
}
