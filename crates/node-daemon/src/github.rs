//! Competitor-gap feature (GitHub write-back): after a successful attempt,
//! push the agent branch to the origin remote, open a PR and (optionally)
//! comment on the linked issue. Everything here is best-effort — callers
//! emit a log event on failure, never fail the task.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// Minimal GitHub REST client with the same timeout posture as the daemon's
/// CP client (`connect_timeout(10s)` + `timeout(120s)`).
fn gh_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()?)
}

/// Push `branch` from the bare mirror clone (`repo_dir`) to GitHub. The token
/// rides only in the remote URL of this one invocation — it is never placed
/// in git config, never logged, and never forwarded to the adapter.
pub fn push_branch(repo_dir: &Path, github_repo: &str, branch: &str, token: &str) -> Result<()> {
    let url = format!(
        "https://x-access-token:{}@github.com/{}.git",
        token, github_repo
    );
    let out = Command::new("git")
        .args(["push", &url, branch])
        .current_dir(repo_dir)
        .output()
        .context("failed to spawn git push")?;
    if !out.status.success() {
        bail!(
            "git push failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Build the PR request body. Pure — unit-tested without network.
pub fn pr_body(
    github_repo: &str,
    branch: &str,
    base: &str,
    task_id: &str,
    attempt_id: &str,
) -> Value {
    json!({
        "title": format!("agentgrid: {branch}"),
        "head": format!("{}:{}", github_repo.split('/').next().unwrap_or(""), branch),
        "base": base,
        "body": format!(
            "Automated change from an agentgrid run.\n\n- task: `{task_id}`\n- attempt: `{attempt_id}`"
        ),
    })
}

/// Parse the `html_url` out of a GitHub `POST /repos/{owner}/{repo}/pulls`
/// response. Pure — unit-tested without network.
pub fn pr_url_from_response(body: &Value) -> Option<String> {
    body.get("html_url")
        .and_then(|u| u.as_str())
        .map(String::from)
}

/// Open a PR via the GitHub REST API. Returns the PR html_url.
pub async fn create_pull_request(
    github_repo: &str,
    branch: &str,
    base: &str,
    task_id: &str,
    attempt_id: &str,
    token: &str,
) -> Result<String> {
    let client = gh_client()?;
    let resp = client
        .post(format!("https://api.github.com/repos/{github_repo}/pulls"))
        .bearer_auth(token)
        .header("User-Agent", "agentgrid")
        .header("Accept", "application/vnd.github+json")
        .json(&pr_body(github_repo, branch, base, task_id, attempt_id))
        .send()
        .await
        .context("PR request failed")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("PR create failed ({status}): {text}");
    }
    let v: Value = resp.json().await.context("parse PR response")?;
    pr_url_from_response(&v).context("PR response missing html_url")
}

/// Comment on a GitHub issue. Returns the comment html_url.
pub async fn comment_issue(
    github_repo: &str,
    issue: i64,
    body: &str,
    token: &str,
) -> Result<String> {
    let client = gh_client()?;
    let resp = client
        .post(format!(
            "https://api.github.com/repos/{github_repo}/issues/{issue}/comments"
        ))
        .bearer_auth(token)
        .header("User-Agent", "agentgrid")
        .header("Accept", "application/vnd.github+json")
        .json(&json!({ "body": body }))
        .send()
        .await
        .context("issue comment request failed")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("issue comment failed ({status}): {text}");
    }
    let v: Value = resp.json().await.context("parse comment response")?;
    v.get("html_url")
        .and_then(|u| u.as_str())
        .map(String::from)
        .context("comment response missing html_url")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_body_uses_owner_branch_head() {
        let b = pr_body("acme/demo", "agent/task-1/1", "main", "t1", "a1");
        assert_eq!(b["head"], "acme:agent/task-1/1");
        assert_eq!(b["base"], "main");
        assert!(b["body"].as_str().unwrap().contains("t1"));
    }

    #[test]
    fn pr_url_parse() {
        let v = json!({ "html_url": "https://github.com/acme/demo/pull/7" });
        assert_eq!(
            pr_url_from_response(&v),
            Some("https://github.com/acme/demo/pull/7".into())
        );
        assert_eq!(pr_url_from_response(&json!({})), None);
    }
}
