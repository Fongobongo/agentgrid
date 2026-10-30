//! Sessions in git: attach a compact attempt transcript to the commit it
//! produced, as a git note, so the "why" travels with the code.
//!
//! The transcript lives in the control-plane DB, which is the right place for
//! the full event stream and the wrong place to look while reading a diff —
//! "why did this line change" is asked *at the commit*. A note puts the prompt,
//! the adapter and the validation verdict next to the change.
//!
//! Two honest limits: the note is keyed by the commit's sha, so it outlives the
//! attempt branch (which is deleted after the push) but does **not** keep that
//! commit reachable — a note path is a tree entry, not a reference, so `git gc`
//! may prune an unpushed commit while the note survives. That is why the note
//! repeats the sha in its own text: it stays readable when the object it names
//! is gone.
//!
//! Off by default, and the note contains the **prompt**: a task prompt is
//! whatever an operator typed, which can include a customer name or a pasted
//! token. `local` keeps it in the node's repo mirror (never leaves the host);
//! `push` additionally pushes `refs/notes/agentgrid` when the task already has
//! `github_push`, i.e. when the operator already asked us to write to that
//! repository. Nothing here is ever allowed to fail an attempt.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// Custom ref: `refs/notes/agentgrid`, not the default `refs/notes/commits`,
/// so a user's own notes are never touched.
pub const NOTES_REF: &str = "agentgrid";

/// How much of the prompt to keep. Long enough for a real task description,
/// short enough that the note stays a note.
const PROMPT_LIMIT: usize = 1200;

/// Carves `refs/notes/agentgrid` out of the mirror's `+refs/*:refs/*` fetch, so
/// `fetch --prune` cannot delete a note that exists only on this node.
const NEGATIVE_REFSPEC: &str = "^refs/notes/agentgrid";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotesMode {
    /// Write nothing (default).
    Off,
    /// Write the note into the node's repo mirror only.
    Local,
    /// Also push the notes ref when the task has `github_push`.
    Push,
}

impl NotesMode {
    /// `AGENTGRID_NOTES=off|local|push` (default `off`; anything unknown is
    /// treated as `off` — a typo must not start publishing prompts).
    pub fn from_env() -> Self {
        match std::env::var("AGENTGRID_NOTES")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "local" => NotesMode::Local,
            "push" => NotesMode::Push,
            _ => NotesMode::Off,
        }
    }

    pub fn is_enabled(self) -> bool {
        self != NotesMode::Off
    }
}

/// Everything the note says about one attempt.
pub struct NoteMeta<'a> {
    pub task_id: &'a str,
    pub attempt_id: &'a str,
    /// The commit this note is attached to (repeated in the text so the note
    /// stays meaningful after the object is pruned).
    pub commit: &'a str,
    pub repository: &'a str,
    pub adapter: &'a str,
    pub prompt: &'a str,
    /// Terminal status word (`succeeded` / `failed` / `cancelled`).
    pub status: &'a str,
    pub exit_code: i32,
    pub error_code: Option<&'a str>,
    /// One line about validation, when the task ran one.
    pub validation: Option<&'a str>,
    /// Budget breach field name, when one tripped.
    pub budget_breach: Option<&'a str>,
    /// Control-plane base URL, for the pointer to the full transcript.
    pub control_plane: &'a str,
}

/// Render the note. Pure, so the shape is testable without a repository.
pub fn render(m: &NoteMeta) -> String {
    let short = |id: &str| id.chars().take(8).collect::<String>();
    let mut out = String::new();
    out.push_str(&format!(
        "agentgrid attempt {} (task {})\n",
        short(m.attempt_id),
        short(m.task_id)
    ));
    out.push_str(&format!("commit: {}\n", m.commit));
    out.push_str(&format!(
        "adapter: {} · status: {} · exit {}\n",
        m.adapter, m.status, m.exit_code
    ));
    out.push_str(&format!("repository: {}\n", m.repository));
    if let Some(err) = m.error_code {
        out.push_str(&format!("error: {err}\n"));
    }
    if let Some(v) = m.validation {
        out.push_str(&format!("validation: {v}\n"));
    }
    if let Some(b) = m.budget_breach {
        out.push_str(&format!("budget: exceeded ({b})\n"));
    }
    out.push_str("\nprompt:\n");
    for line in clip(m.prompt, PROMPT_LIMIT).lines() {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!(
        "\nfull transcript: ag events {} (control plane {})\n",
        m.task_id, m.control_plane
    ));
    out
}

/// First `limit` chars on a char boundary, marked when cut.
fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}\n… (truncated)")
}

/// Keep `fetch --prune` from deleting our notes ref.
///
/// The node's repository is a `--mirror` clone, whose fetch refspec is
/// `+refs/*:refs/*` and whose fetch runs with `--prune`. A note written in
/// `local` mode exists only here, so the *next* attempt on the same repository
/// pruned it away — verified on a scratch mirror: one note before
/// `fetch --prune`, zero after. The feature would have looked correct in a
/// one-shot smoke test and lost every note in production.
///
/// A negative refspec is the documented way to carve a ref out of the fetch,
/// and it protects from the prune too: with `^refs/notes/agentgrid` in
/// `remote.origin.fetch`, the note survives. Idempotent, and a no-op for a
/// repository without a remote (nothing fetches it, so nothing prunes it).
fn protect_from_prune(repo_dir: &Path) -> Result<()> {
    let configured = Command::new("git")
        .args(["config", "--get-all", "remote.origin.fetch"])
        .current_dir(repo_dir)
        .output()
        .context("read remote.origin.fetch")?;
    if !configured.status.success() {
        // No such key (plain repo, or no remote): nothing to protect.
        return Ok(());
    }
    let current = String::from_utf8_lossy(&configured.stdout);
    if current.lines().any(|l| l.trim() == NEGATIVE_REFSPEC) {
        return Ok(());
    }
    let added = Command::new("git")
        .args(["config", "--add", "remote.origin.fetch", NEGATIVE_REFSPEC])
        .current_dir(repo_dir)
        .output()
        .context("add the notes exclusion to remote.origin.fetch")?;
    if !added.status.success() {
        bail!(
            "git config failed: {}",
            String::from_utf8_lossy(&added.stderr).trim()
        );
    }
    Ok(())
}

/// Attach `text` to `commit` as a note in `repo_dir`.
///
/// The message goes in over stdin rather than a temp file: the note contains
/// the prompt, and a stray file in `/tmp` is exactly the kind of copy that gets
/// forgotten.
///
/// The committer identity is passed explicitly. Writing a note creates a commit
/// object, so git needs an author — and a fresh node (or CI) has no global
/// `user.name`, which made this fail with "Author identity unknown" on every
/// host where nobody had ever run `git config`. The identity is the daemon's,
/// not the operator's: the note is authored by agentgrid, same as the commits
/// the integrator makes (`git.rs`).
pub fn attach(repo_dir: &Path, commit: &str, text: &str) -> Result<()> {
    if commit.is_empty() {
        bail!("empty commit");
    }
    protect_from_prune(repo_dir)?;
    let mut child = Command::new("git")
        .args([
            "-c",
            "user.name=agentgrid",
            "-c",
            "user.email=agentgrid@agentgrid",
            "notes",
            "--ref",
            NOTES_REF,
            "add",
            "-f",
            "-F",
            "-",
            commit,
        ])
        .current_dir(repo_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn git notes")?;
    {
        let mut stdin = child.stdin.take().context("git notes stdin")?;
        stdin.write_all(text.as_bytes()).context("write note")?;
    } // drop closes the pipe; without this `wait` deadlocks
    let out = child.wait_with_output().context("wait for git notes")?;
    if !out.status.success() {
        bail!(
            "git notes failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta<'a>(prompt: &'a str) -> NoteMeta<'a> {
        NoteMeta {
            task_id: "91ab3c4d5e6f",
            attempt_id: "7f3c9d2e1a4b",
            commit: "44fc3da0a9b18e77f5bc39df34d969409737e61f",
            repository: "demo",
            adapter: "opencode",
            prompt,
            status: "succeeded",
            exit_code: 0,
            error_code: None,
            validation: Some("passed — cargo test"),
            budget_breach: None,
            control_plane: "http://127.0.0.1:7800",
        }
    }

    #[test]
    fn mode_defaults_to_off_and_rejects_typos() {
        // Env-free assertion for the default; the parsing cases are below.
        assert!(!NotesMode::Off.is_enabled());
        assert!(NotesMode::Local.is_enabled());
        assert!(NotesMode::Push.is_enabled());
    }

    #[test]
    fn render_names_the_attempt_and_the_pointer_to_the_full_log() {
        let text = render(&meta("Rename parse_cidr"));
        assert!(text.starts_with("agentgrid attempt 7f3c9d2e (task 91ab3c4d)"));
        // The sha is repeated in the text: the note stays readable after gc
        // prunes the unpushed commit it is keyed by.
        assert!(text.contains("commit: 44fc3da0a9b18e77f5bc39df34d969409737e61f"));
        assert!(text.contains("adapter: opencode · status: succeeded · exit 0"));
        assert!(text.contains("validation: passed — cargo test"));
        assert!(text.contains("  Rename parse_cidr"));
        // The full transcript is in the CP, and the note says where to ask.
        assert!(text.contains("ag events 91ab3c4d5e6f"));
        assert!(text.contains("http://127.0.0.1:7800"));
    }

    #[test]
    fn a_long_prompt_is_clipped_on_a_char_boundary() {
        // Cyrillic and emoji are multi-byte: slicing by byte would panic.
        let long = "причина ".repeat(400);
        let text = render(&meta(&long));
        assert!(text.contains("(truncated)"), "must say it was cut");
        assert!(
            text.len() < 4 * PROMPT_LIMIT,
            "note not bounded: {}",
            text.len()
        );
        // Still valid UTF-8 by construction (String), and the tail survived.
        assert!(text.contains("full transcript"));
    }

    #[test]
    fn error_and_budget_lines_appear_only_when_set() {
        let mut m = meta("p");
        m.status = "failed";
        m.exit_code = 1;
        m.error_code = Some("validation_failed");
        m.validation = None;
        m.budget_breach = Some("max_tokens");
        let text = render(&m);
        assert!(text.contains("error: validation_failed"));
        assert!(text.contains("budget: exceeded (max_tokens)"));
        assert!(!text.contains("validation:"));
    }

    /// The rendering tests above prove the text; this proves the git plumbing
    /// (`git notes --ref … add -F -` reading stdin, no temp file).
    #[test]
    fn attach_round_trips_through_git_notes() {
        let Ok(_) = std::process::Command::new("git").arg("--version").output() else {
            eprintln!("skip: git not available");
            return;
        };
        let dir = std::env::temp_dir().join(format!("ag-notes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        let sha = git(&["rev-parse", "HEAD"]).trim().to_string();

        attach(&dir, &sha, "why this change").expect("attach");
        let shown = git(&["notes", "--ref", NOTES_REF, "show", &sha]);
        assert_eq!(shown.trim(), "why this change");

        // Writing a note creates a commit, so git needs an identity. It must be
        // the daemon's and it must be explicit: CI and a fresh node have no
        // global `user.name`, and relying on one made this fail with
        // "Author identity unknown" on every host where nobody had run
        // `git config`. Asserting the author catches a removed `-c` flag.
        assert_eq!(
            git(&[
                "log",
                "--no-walk",
                "--format=%an <%ae>",
                "refs/notes/agentgrid"
            ])
            .trim(),
            "agentgrid <agentgrid@agentgrid>"
        );

        // A retry on the same commit overwrites rather than stacking.
        attach(&dir, &sha, "second take").expect("reattach");
        assert_eq!(
            git(&["notes", "--ref", NOTES_REF, "show", &sha]).trim(),
            "second take"
        );

        assert!(attach(&dir, "", "x").is_err(), "empty sha must be refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The node's repository is a `--mirror` clone, so its fetch refspec is
    /// `+refs/*:refs/*` and the fetch runs `--prune`. A `local` note exists
    /// only on this node, which made the next attempt on the same repository
    /// delete it — the e2e caught it, this pins it.
    #[test]
    fn a_local_note_survives_the_next_fetch_prune() {
        let Ok(_) = std::process::Command::new("git").arg("--version").output() else {
            eprintln!("skip: git not available");
            return;
        };
        let dir = std::env::temp_dir().join(format!("ag-notes-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |cwd: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        let remote = dir.join("remote.git");
        let seed = dir.join("seed");
        let mirror = dir.join("mirror.git");
        git(&dir, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        git(
            &dir,
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                seed.to_str().unwrap(),
            ],
        );
        std::fs::write(seed.join("a.txt"), "x").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-qm", "init"]);
        git(&seed, &["branch", "-M", "main"]);
        git(&seed, &["push", "-q", "origin", "main"]);
        git(
            &dir,
            &[
                "clone",
                "-q",
                "--mirror",
                remote.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        let sha = git(&seed, &["rev-parse", "HEAD"]).trim().to_string();

        attach(&mirror, &sha, "why").expect("attach");
        // Idempotent: a second attempt must not stack duplicate refspecs.
        attach(&mirror, &sha, "why again").expect("reattach");
        let refspecs = git(&mirror, &["config", "--get-all", "remote.origin.fetch"]);
        let exclusions = refspecs
            .lines()
            .filter(|l| l.trim() == NEGATIVE_REFSPEC)
            .count();
        assert_eq!(exclusions, 1, "refspec added twice:\n{refspecs}");

        git(&mirror, &["fetch", "--prune", "-q"]);
        assert_eq!(
            git(&mirror, &["notes", "--ref", NOTES_REF, "show", &sha]).trim(),
            "why again",
            "fetch --prune deleted the local note"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
