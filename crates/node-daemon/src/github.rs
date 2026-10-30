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
    let url = format!("https://github.com/{github_repo}.git");
    push_ref(repo_dir, &url, branch, token)
}

/// Push any refspec to `url` with the token fed through a 0600 askpass helper.
///
/// The helper exists because `https://x-access-token:<token>@…` in the URL puts
/// the token in `git`'s argv, visible to any co-tenant reading
/// `/proc/<pid>/cmdline`. `refspec` may be a branch or a full ref
/// (`refs/notes/agentgrid`); it is the caller's job to have validated anything
/// that came from task data.
pub fn push_ref(repo_dir: &Path, url: &str, refspec: &str, token: &str) -> Result<()> {
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
    let out = Command::new("git")
        .args(["push", url, refspec])
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
    let (owner, name) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let ok_part = |s: &str| {
        !s.is_empty()
            && s.len() <= 128
            && s.chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    };
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

/// Project assembly: API base override for tests (`AGENTGRID_GITHUB_API_URL`,
/// default the public API). Lets unit tests drive the client against a dummy
/// server instead of the network.
pub fn api_base() -> String {
    std::env::var("AGENTGRID_GITHUB_API_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string())
}

/// Split a validated `owner/name` into its parts. The caller validates first
/// (`validate_push_target`); a malformed value is a bug, surfaced as an
/// error rather than a crafted URL.
fn split_owner_repo(github_repo: &str) -> Result<(&str, &str)> {
    let mut parts = github_repo.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(o), Some(n), None) if !o.is_empty() && !n.is_empty() => Ok((o, n)),
        _ => bail!("invalid github_repo (want owner/name): {github_repo}"),
    }
}

/// Project assembly: does `owner/name` exist? `Ok(true)` on 200, `Ok(false)`
/// on 404, `Err` on anything else (auth/network/server) — an ambiguous
/// answer must not trigger a create (fail-closed: no duplicate-attempt
/// against a repo we cannot see).
pub async fn repo_exists(github_repo: &str, token: &str, proxy: Option<&str>) -> Result<bool> {
    let client = gh_client(proxy)?;
    let resp = client
        .get(format!("{}/repos/{github_repo}", api_base()))
        .bearer_auth(token)
        .header("User-Agent", "agentgrid")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("repo lookup failed")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(false);
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("repo lookup failed ({status}): {text}");
    }
    Ok(true)
}

/// Project assembly: create a private, empty `owner/name` via the API.
/// Resolves the owner kind first (`GET /user` → login): a personal repo goes
/// to `POST /user/repos`, an org repo to `POST /orgs/{owner}/repos`. Always
/// private + `auto_init: false` — the node pushes the first branch itself,
/// and a public-by-default create would leak an operator's project.
pub async fn create_repository(github_repo: &str, token: &str, proxy: Option<&str>) -> Result<()> {
    let (owner, name) = split_owner_repo(github_repo)?;
    let client = gh_client(proxy)?;
    let base = api_base();
    let me: Value = client
        .get(format!("{base}/user"))
        .bearer_auth(token)
        .header("User-Agent", "agentgrid")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("owner lookup failed")?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("owner lookup failed: {e}"))?
        .json()
        .await
        .context("parse owner response")?;
    let login = me.get("login").and_then(|l| l.as_str()).unwrap_or("");
    let url = if login.eq_ignore_ascii_case(owner) {
        format!("{base}/user/repos")
    } else {
        format!("{base}/orgs/{owner}/repos")
    };
    let resp = client
        .post(url)
        .bearer_auth(token)
        .header("User-Agent", "agentgrid")
        .header("Accept", "application/vnd.github+json")
        .json(&json!({ "name": name, "private": true, "auto_init": false }))
        .send()
        .await
        .context("repo create failed")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("repo create failed ({status}): {text}");
    }
    Ok(())
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

    #[test]
    fn split_owner_repo_accepts_shapes_rejects_garbage() {
        assert_eq!(split_owner_repo("acme/demo").unwrap(), ("acme", "demo"));
        assert!(split_owner_repo("acme").is_err());
        assert!(split_owner_repo("a/b/c").is_err());
        assert!(split_owner_repo("/b").is_err());
        assert!(split_owner_repo("").is_err());
    }

    /// Process-global `AGENTGRID_GITHUB_API_URL` is shared across tests;
    /// serialize the client tests so parallel runs cannot point each other
    /// at the wrong dummy server (mirrors `sandbox::tests::ENV_LOCK`, as a
    /// tokio mutex since the guard spans awaits).
    static API_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Minimal dummy GitHub API over raw TCP (house pattern): routes by
    /// request line, records every request line + body for assertions.
    /// `GET /repos/acme/new` → 404, `GET /repos/acme/exists` → 200,
    /// `GET /repos/acme/broken` → 500, `GET /user` → login `acme`,
    /// `POST /user/repos` and `POST /orgs/other/repos` → 201.
    async fn dummy_github_api(seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    break;
                };
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    loop {
                        // Read headers.
                        let mut head = Vec::new();
                        loop {
                            match s.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => {
                                    head.extend_from_slice(&buf[..n]);
                                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                                        break;
                                    }
                                }
                            }
                        }
                        let h = String::from_utf8_lossy(&head);
                        let request_line = h.lines().next().unwrap_or_default().to_string();
                        let len: usize = h
                            .lines()
                            .find_map(|l| {
                                l.strip_prefix("Content-Length:")
                                    .or_else(|| l.strip_prefix("content-length:"))
                                    .and_then(|v| v.trim().parse().ok())
                            })
                            .unwrap_or(0);
                        let mut body = vec![0u8; len];
                        let mut read = 0;
                        // Bytes past the header already sit in `head`.
                        if let Some(pos) = head.windows(4).position(|w| w == b"\r\n\r\n") {
                            let extra = &head[pos + 4..];
                            let take = extra.len().min(len);
                            body[..take].copy_from_slice(&extra[..take]);
                            read = take;
                        }
                        while read < len {
                            match s.read(&mut body[read..]).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => read += n,
                            }
                        }
                        seen.lock()
                            .unwrap()
                            .push(format!("{request_line} {}", String::from_utf8_lossy(&body)));
                        let (status, payload): (&str, &str) =
                            if request_line.starts_with("GET /repos/acme/new ") {
                                ("404 Not Found", "{}")
                            } else if request_line.starts_with("GET /repos/acme/exists ") {
                                ("200 OK", "{}")
                            } else if request_line.starts_with("GET /repos/acme/broken ") {
                                ("500 Internal Server Error", "{}")
                            } else if request_line.starts_with("GET /user ") {
                                ("200 OK", r#"{"login":"acme"}"#)
                            } else if request_line.starts_with("POST /user/repos ")
                                || request_line.starts_with("POST /orgs/other/repos ")
                            {
                                ("201 Created", "{}")
                            } else if request_line.starts_with("POST ") {
                                ("422 Unprocessable Entity", "{}")
                            } else {
                                ("404 Not Found", "{}")
                            };
                        let resp = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: keep-alive\r\n\r\n{payload}",
                            len = payload.len(),
                        );
                        if s.write_all(resp.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn repo_exists_maps_200_404_and_errors() {
        let _g = API_ENV_LOCK.lock().await;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let base = dummy_github_api(seen).await;
        std::env::set_var("AGENTGRID_GITHUB_API_URL", &base);
        assert!(repo_exists("acme/exists", "tok", None).await.unwrap());
        assert!(!repo_exists("acme/new", "tok", None).await.unwrap());
        assert!(repo_exists("acme/broken", "tok", None).await.is_err());
        std::env::remove_var("AGENTGRID_GITHUB_API_URL");
    }

    #[tokio::test]
    async fn create_repository_posts_private_to_user_or_org() {
        let _g = API_ENV_LOCK.lock().await;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let base = dummy_github_api(seen.clone()).await;
        std::env::set_var("AGENTGRID_GITHUB_API_URL", &base);
        // Owner == login → POST /user/repos, always private + no auto-init.
        create_repository("acme/new", "tok", None).await.unwrap();
        // Owner != login → POST /orgs/{owner}/repos.
        create_repository("other/new", "tok", None).await.unwrap();
        // Unprocessable → Err (no silent success).
        assert!(create_repository("bogus/new", "tok", None).await.is_err());
        std::env::remove_var("AGENTGRID_GITHUB_API_URL");
        let log = seen.lock().unwrap().join("\n");
        let user_post = log
            .lines()
            .find(|l| l.starts_with("POST /user/repos "))
            .expect("user-repo create posted");
        assert!(user_post.contains(r#""private":true"#), "{user_post}");
        assert!(user_post.contains(r#""auto_init":false"#), "{user_post}");
        assert!(user_post.contains(r#""name":"new""#), "{user_post}");
        assert!(
            log.lines()
                .any(|l| l.starts_with("POST /orgs/other/repos ")),
            "org-repo create posted:\n{log}"
        );
    }

    /// `push_ref` exists so a notes ref can travel the same way a branch does.
    /// A `file://` remote exercises the whole path (askpass helper, argv, ref
    /// creation) without a GitHub token; git ignores the credentials.
    #[test]
    fn push_ref_carries_a_notes_ref_to_the_remote() {
        let dir = std::env::temp_dir().join(format!("ag-pushref-{}", std::process::id()));
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
        let Ok(_) = std::process::Command::new("git").arg("--version").output() else {
            eprintln!("skip: git not available");
            return;
        };
        let remote = dir.join("remote.git");
        let work = dir.join("work");
        git(&dir, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        git(&dir, &["init", "-q", "-b", "main", work.to_str().unwrap()]);
        std::fs::write(work.join("a.txt"), "x").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-qm", "init"]);
        let sha = git(&work, &["rev-parse", "HEAD"]).trim().to_string();
        crate::notes::attach(&work, &sha, "why").expect("attach");

        let url = format!("file:///{}", remote.display()).replace('\\', "/");
        push_ref(&work, &url, "refs/notes/agentgrid", "unused-token").expect("push notes ref");

        // The remote now has the note ref, pointing at a commit that carries
        // the note — that is the whole point of pushing it.
        let remote_sha = git(&remote, &["rev-parse", "refs/notes/agentgrid"])
            .trim()
            .to_string();
        assert_eq!(remote_sha.len(), 40, "notes ref missing on remote");
        assert_eq!(
            git(&remote, &["notes", "--ref", "agentgrid", "show", &sha]).trim(),
            "why"
        );

        // A refspec the remote cannot accept must surface git's own error, not
        // report success.
        assert!(push_ref(&work, &url, "refs/notes/nope", "tok").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
