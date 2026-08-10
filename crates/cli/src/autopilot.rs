//! Plan 2.6 (#22c): overnight autopilot.
//!
//! `ag autopilot "<objective>" --repository <repo> --validate "<cmd>"
//! --local-path <git checkout>` submits a task per iteration against the
//! control plane, waits for a terminal status, and commits the result into
//! the local checkout. A failed iteration rolls the local head back to the
//! last known-good commit; the loop stops at `max_iterations`, the deadline,
//! or the first terminal failure. `<summary-root>/<objective-slug>/SUMMARY.md`
//! captures the trail.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentgrid_common::{CreateTaskRequest, TaskStatus, TaskView};
use anyhow::{Context as _, Result};
use chrono::Utc;

/// One autopilot iteration: a tracked task id + its terminal status.
#[derive(Debug, Clone)]
pub struct Iteration {
    pub n: u32,
    pub task_id: String,
    pub status: TaskStatus,
    pub commit: Option<String>,
    pub summary: String,
}

/// Final summary for the whole autopilot run — written to SUMMARY.md.
#[derive(Debug)]
pub struct AutopilotReport {
    pub objective: String,
    pub started_at: chrono::DateTime<Utc>,
    pub finished_at: chrono::DateTime<Utc>,
    pub iterations: Vec<Iteration>,
    pub final_commit: Option<String>,
    pub succeeded: bool,
}

impl AutopilotReport {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("# Autopilot summary: {}\n\n", self.objective));
        out.push_str(&format!("- Started: {}\n", self.started_at));
        out.push_str(&format!("- Finished: {}\n", self.finished_at));
        out.push_str(&format!(
            "- Outcome: {}\n",
            if self.succeeded {
                "'completed'"
            } else {
                "'stopped'"
            }
        ));
        if let Some(c) = &self.final_commit {
            out.push_str(&format!("- Final commit: `{c}`\n"));
        }
        out.push_str("\n## Iterations\n");
        for it in &self.iterations {
            let status = format!("{:?}", it.status);
            out.push_str(&format!(
                "- iter {} task `{}` → {}{} ({})\n",
                it.n,
                it.task_id,
                status,
                it.commit
                    .as_deref()
                    .map(|c| format!(" commit `{c}`"))
                    .unwrap_or_default(),
                it.summary
            ));
        }
        out
    }
}

/// Autopilot loop configuration — keeps `run_autopilot` at 1 arg after the
/// transport so clippy stops shouting.
pub struct AutopilotOpts<'a> {
    pub objective: &'a str,
    pub repository: &'a str,
    pub adapter: &'a str,
    pub validate: Option<&'a str>,
    pub local_path: &'a Path,
    pub max_iterations: u32,
    pub max_duration: Duration,
    pub summary_root: &'a Path,
}

/// Run the autopilot loop. Existing transport (with the bearer header
/// pre-attached) is reused from the CLI `main` so token plumbing is shared.
pub async fn run_autopilot(
    client: &reqwest::Client,
    base: &str,
    o: &AutopilotOpts<'_>,
) -> Result<AutopilotReport> {
    let started = Utc::now();
    let deadline = Instant::now() + o.max_duration;
    let mut iterations: Vec<Iteration> = Vec::new();
    let mut last_good: Option<String> = git_head(o.local_path);
    let mut succeeded = false;

    for n in 1..=o.max_iterations.max(1) {
        if Instant::now() >= deadline {
            break;
        }
        let prompt = iteration_prompt(o.objective, n, &iterations);
        let req = CreateTaskRequest {
            prompt: prompt.clone(),
            repository: o.repository.into(),
            adapter: o.adapter.into(),
            validation_command: o.validate.map(|s| s.to_string()),
            ..Default::default()
        };
        let task_id = submit_task(client, base, &req).await?;
        let status = wait_terminal(client, base, &task_id).await?;
        let commit = if status == TaskStatus::Succeeded {
            commit_iteration(o.local_path, n, &format!("{} (iter {n})", o.objective))?
        } else {
            rollback_to(o.local_path, last_good.as_deref())?;
            None
        };
        let summary = iteration_summary(&status, commit.as_deref());
        iterations.push(Iteration {
            n,
            task_id: task_id.clone(),
            status,
            commit,
            summary,
        });
        if status == TaskStatus::Succeeded {
            last_good = git_head(o.local_path);
            succeeded = true;
        } else if matches!(status, TaskStatus::Failed | TaskStatus::Cancelled) {
            break;
        }
    }
    let finished = Utc::now();
    let report = AutopilotReport {
        objective: o.objective.into(),
        started_at: started,
        finished_at: finished,
        iterations,
        final_commit: last_good,
        succeeded,
    };
    write_summary(o.summary_root, &report)?;
    Ok(report)
}

/// Where to write the summary: `<workspace>/<objective-slug>/SUMMARY.md`.
pub fn summary_path(workspace_root: &Path, objective: &str) -> PathBuf {
    workspace_root
        .join(objective_slug(objective))
        .join("SUMMARY.md")
}

/// Public for tests and operators computing the path.
pub fn write_summary(workspace_root: &Path, report: &AutopilotReport) -> Result<PathBuf> {
    let path = summary_path(workspace_root, &report.objective);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create summary dir")?;
    }
    std::fs::write(&path, report.render()).context("write SUMMARY.md")?;
    Ok(path)
}

fn iteration_prompt(objective: &str, n: u32, iterations: &[Iteration]) -> String {
    let mut p = String::new();
    p.push_str(&format!("Objective: {objective}\n"));
    p.push_str(&format!("Iteration {n} of the autopilot run.\n"));
    if !iterations.is_empty() {
        p.push_str("Prior iterations (most recent last):\n");
        for it in iterations {
            let st = format!("{:?}", it.status);
            p.push_str(&format!(
                "- iter {}: {}{}\n",
                it.n,
                st,
                it.commit
                    .as_deref()
                    .map(|c| format!(" committed {}", &c[..8.min(c.len())]))
                    .unwrap_or_default()
            ));
            if !it.summary.is_empty() {
                p.push_str(&format!("  note: {}\n", it.summary));
            }
        }
    }
    p
}

fn iteration_summary(status: &TaskStatus, commit: Option<&str>) -> String {
    match (status, commit) {
        (TaskStatus::Succeeded, Some(c)) => format!("passed; committed {}", &c[..8.min(c.len())]),
        (TaskStatus::Succeeded, None) => "passed; no changes to commit".into(),
        (TaskStatus::Failed, _) => "failed; rolled back to last good commit".into(),
        (TaskStatus::Cancelled, _) => "cancelled; rolled back".into(),
        _ => format!("status {status:?}"),
    }
}

async fn submit_task(
    client: &reqwest::Client,
    base: &str,
    req: &CreateTaskRequest,
) -> Result<String> {
    let resp = client
        .post(format!("{base}/v1/tasks"))
        .json(req)
        .send()
        .await
        .context("submit task")?;
    if !resp.status().is_success() {
        anyhow::bail!("submit task failed ({})", resp.status());
    }
    let v: serde_json::Value = resp.json().await.context("parse task response")?;
    Ok(v["id"].as_str().context("task id missing")?.to_string())
}

async fn wait_terminal(client: &reqwest::Client, base: &str, task_id: &str) -> Result<TaskStatus> {
    loop {
        let resp = client
            .get(format!("{base}/v1/tasks/{task_id}"))
            .send()
            .await
            .context("poll task")?;
        if !resp.status().is_success() {
            anyhow::bail!("poll task failed ({})", resp.status());
        }
        let view: TaskView = resp.json().await.context("parse task view")?;
        match view.status {
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled => {
                return Ok(view.status)
            }
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
}

pub fn git_head(workdir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workdir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn commit_iteration(workdir: &Path, n: u32, msg: &str) -> Result<Option<String>> {
    let _ = std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(workdir)
        .status();
    let dirty = !std::process::Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(workdir)
        .status()
        .map(|s| s.success())
        .unwrap_or(true);
    if !dirty {
        return Ok(None);
    }
    let st = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=agentgrid-autopilot",
            "-c",
            "user.email=autopilot@agentgrid.local",
            "commit",
            "-m",
            &format!("autopilot iter {n}: {msg}"),
        ])
        .current_dir(workdir)
        .status()
        .context("git commit failed")?;
    if !st.success() {
        anyhow::bail!("git commit exit {st}");
    }
    Ok(git_head(workdir))
}

fn rollback_to(workdir: &Path, sha: Option<&str>) -> Result<()> {
    if !workdir.join(".git").exists() {
        return Ok(());
    }
    if let Some(s) = sha {
        let st = std::process::Command::new("git")
            .args(["reset", "--hard", s])
            .current_dir(workdir)
            .status()
            .context("git reset --hard failed")?;
        if !st.success() {
            anyhow::bail!("git reset --hard exit {st}");
        }
    }
    let _ = std::process::Command::new("git")
        .args(["clean", "-fd"])
        .current_dir(workdir)
        .status();
    Ok(())
}

pub fn objective_slug(objective: &str) -> String {
    let raw: String = objective
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    raw.split('-')
        .filter(|s| !s.is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn report(iters: Vec<Iteration>, succeeded: bool) -> AutopilotReport {
        AutopilotReport {
            objective: "Fix flaky tests".into(),
            started_at: Utc.with_ymd_and_hms(2026, 5, 10, 22, 0, 0).unwrap(),
            finished_at: Utc.with_ymd_and_hms(2026, 5, 11, 6, 0, 0).unwrap(),
            iterations: iters,
            final_commit: Some("abc123def456".into()),
            succeeded,
        }
    }

    fn iter(n: u32, status: TaskStatus, commit: Option<&str>) -> Iteration {
        Iteration {
            n,
            task_id: format!("t-{n}"),
            status,
            commit: commit.map(str::to_string),
            summary: iteration_summary(&status, commit),
        }
    }

    #[test]
    fn slug_lowercases_and_strips_non_alnum() {
        assert_eq!(objective_slug("Fix flaky tests"), "fix-flaky-tests");
        assert_eq!(objective_slug("cleanup  NOW!"), "cleanup-now");
    }

    #[test]
    fn report_renders_iterations_and_outcome() {
        let r = report(
            vec![
                iter(1, TaskStatus::Succeeded, Some("aaaa1111")),
                iter(2, TaskStatus::Failed, None),
            ],
            false,
        );
        let md = r.render();
        assert!(md.contains("# Autopilot summary: Fix flaky tests"));
        assert!(md.contains("iter 1"));
        assert!(md.contains("iter 2"));
        assert!(md.contains("'stopped'")); // failed → not 'completed'
        assert!(md.contains("Final commit: `abc123def456`"));
    }

    #[test]
    fn summary_marks_rollback_on_failure() {
        let s = iteration_summary(&TaskStatus::Failed, None);
        assert!(s.contains("rolled back"));
        let s = iteration_summary(&TaskStatus::Succeeded, Some("cafe0001"));
        assert!(s.contains("passed"));
        assert!(s.contains("cafe0001"));
    }

    #[test]
    fn summary_path_uses_slug_subdir() {
        let root = std::path::Path::new("/tmp/agentgrid-workspace");
        let p = summary_path(root, "Fix flaky tests");
        assert_eq!(p, root.join("fix-flaky-tests").join("SUMMARY.md"));
    }
}
