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
fn gh_client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120));
    // Egress proxy pool: route GitHub API traffic through the node pool.
    if let Some(p) = proxy {
        b = b.proxy(reqwest::Proxy::all(p)?);
    }
    Ok(b.build()?)
}

/// Push `branch` from the bare mirror clone (`repo_dir`) to GitHub.
///
/// Audit X-N6: both the token channel and the argv inputs were unsafe.
/// The token used to ride in the remote URL, which is world-readable via
/// `/proc/<pid>/cmdline` to co-tenants — it now travels through a 0600
/// askpass helper fed by env instead. `github_repo`/`branch` come from task
/// data, so they are validated up front (option-injection like
/// `--upload-pack=...` and malformed `owner/name` shapes fail here, not in
/// git's own error text).
pub fn push_branch(repo_dir: &Path, github_repo: &str, branch: &str, token: &str) -> Result<()> {
    validate_push_target(github_repo, branch)?;
    let ask_dir = std::env::temp_dir().join(format!(
        "ag-askpass-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&ask_dir).context("create askpass dir")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ask_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let helper = ask_dir.join("askpass.sh");
    std::fs::write(&helper, "#!/bin/sh\necho \"$AGENTGRID_GH_ASKPASS_TOKEN\"\n")
        .context("write askpass helper")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o600))?;
    }
    let url = format!("https://github.com/{github_repo}.git");
    let out = Command::new("git")
        .args(["push", &url, branch])
        .current_dir(repo_dir)
        .env("GIT_ASKPASS", &helper)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("AGENTGRID_GH_ASKPASS_TOKEN", token)
        .output()
        .context("failed to spawn git push")?;
    let _ = std::fs::remove_dir_all(&ask_dir);
    if !out.status.success() {
        bail!(
            "git push failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn validate_push_target(github_repo: &str, branch: &str) -> Result<()> {
    let mut parts = github_repo.split('/');
    let (owner, name) = (
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
    );
    let ok_part =
        |s: &str| !s.is_empty() && s.len() <= 128 && s.chars().all(|c| {
            c.is_alphanumeric() || c == '-' || c == '_' || c == '.'
        });
    if parts.next().is_some() || !ok_part(owner) || !ok_part(name) {
        bail!("invalid github_repo (want owner/name): {github_repo}");
    }
    if branch.is_empty()
        || branch.len() > 256
        || branch.starts_with('-')
        || branch.starts_with('/')
        || branch.contains("..")
        || branch.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        bail!("invalid branch name");
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
        // Audit X-N6 (S7 residue): `github_repo` without a slash used to
        // render a bogus `demo:branch` head. Same-repo PRs accept a bare
        // branch, so fall back to it instead of fabricating an owner.
        "head": if github_repo.contains('/') {
            format!("{}:{}", github_repo.split('/').next().unwrap_or(""), branch)
        } else {
            branch.to_string()
        },
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
    proxy: Option<&str>,
) -> Result<String> {
    let client = gh_client(proxy)?;
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
    proxy: Option<&str>,
) -> Result<String> {
    let client = gh_client(proxy)?;
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
