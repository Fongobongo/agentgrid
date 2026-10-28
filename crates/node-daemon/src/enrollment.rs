//! Node enrollment: credential persistence, token scrubbing.

use anyhow::Result;

use crate::config::{adapter_permission_interception, Config, SavedCredential};
use agentgrid_common::{EnrollRequest, EnrollResponse};

/// Load saved credential or enroll a new node. Returns the credential for
/// subsequent heartbeats/polls.
pub async fn load_or_enroll(cfg: &Config) -> Result<SavedCredential> {
    if let Ok(data) = std::fs::read_to_string(&cfg.credential_path) {
        if let Ok(cred) = serde_json::from_str::<SavedCredential>(&data) {
            return Ok(cred);
        }
    }
    let cred = enroll_node(cfg).await?;
    // Node credential is secret material: 0600 regardless of umask (matches
    // the env-file scrub path), including for a pre-existing file.
    // (0600 is a POSIX concept; on Windows the best effort is a normal
    // private write — ACL hardening is out of scope for a dev host.)
    let data = serde_json::to_vec(&cred)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&cfg.credential_path)?;
        f.write_all(&data)?;
        std::fs::set_permissions(&cfg.credential_path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    std::fs::write(&cfg.credential_path, &data)?;
    scrub_enroll_token_from_env(cfg).await;
    Ok(cred)
}

/// Enroll this node with the control plane.
async fn enroll_node(cfg: &Config) -> Result<SavedCredential> {
    let client = crate::polling::daemon_http_client()?;
    let body = EnrollRequest {
        token: cfg.enroll_token.clone().unwrap_or_default(),
        name: cfg.node_name.clone(),
        adapters: cfg.adapters.iter().map(|a| a.id.clone()).collect(),
        repositories: cfg.repositories.clone(),
        max_concurrency: cfg.max_concurrency,
        agent_version: cfg.agent_version.clone(),
        protocol_version: None,
        permission_interception: cfg
            .adapters
            .first()
            .map(adapter_permission_interception)
            .unwrap_or_else(|| "wrapper".into()),
    };
    // With `AGENTGRID_SERVERS` the first configured URL may be a standby:
    // follow the leader hint (or rotate) and retry instead of failing boot.
    let attempts = cfg.endpoints.urls().len().max(1) * 2;
    let mut last = String::new();
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        let resp = client
            .post(format!("{}/v1/node/enroll", cfg.base()))
            .json(&body)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let enrolled: EnrollResponse = r.json().await?;
                return Ok(SavedCredential {
                    node_id: enrolled.node_id,
                    credential: enrolled.credential,
                });
            }
            Ok(r) => {
                let status = r.status();
                let hint = r
                    .headers()
                    .get("x-agentgrid-leader")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                last = r.text().await.unwrap_or_default();
                let followed = cfg.endpoints.follow_hint(hint.as_deref());
                if !followed && cfg.endpoints.urls().len() > 1 {
                    cfg.endpoints.advance();
                }
                if status != reqwest::StatusCode::SERVICE_UNAVAILABLE {
                    break;
                }
                tracing::warn!(
                    "enroll: control plane is not the leader; retrying at {}",
                    cfg.base()
                );
            }
            Err(e) => {
                last = e.to_string();
                if cfg.endpoints.urls().len() > 1 {
                    let next = cfg.endpoints.advance();
                    tracing::warn!("enroll: control plane unreachable; rotating to {next}");
                }
            }
        }
    }
    anyhow::bail!("enroll failed: {}", last)
}

/// After successful enrollment, scrub `AGENTGRID_ENROLL_TOKEN` from the
/// operator-provisioned env file so a rebooting node reuses the persisted
/// credential and the token can't be leaked off disk. Atomic (temp+rename)
/// so a crash mid-write never leaves a truncated env file; the temp gets
/// 0600 so the token can't be read by other users between write and rename.
pub async fn scrub_enroll_token_from_env(cfg: &Config) {
    let Some(path) = &cfg.env_file else {
        return;
    };
    scrub_token_from_file(path).await;
}

/// Remove any `AGENTGRID_ENROLL_TOKEN=...` line from `path` in place (atomic
/// temp+rename, perm 0600). No-op if the file is missing; logged failures are
/// never fatal (the credential is already persisted).
pub async fn scrub_token_from_file(path: &std::path::Path) {
    let Ok(text) = tokio::fs::read_to_string(path).await else {
        return;
    };
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("AGENTGRID_ENROLL_TOKEN="))
        .collect();
    let mut new = kept.join("\n");
    if !new.is_empty() {
        new.push('\n');
    }
    let tmp = path.with_extension("tmp.scrub");
    if tokio::fs::write(&tmp, &new).await.is_err() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await;
    }
    if tokio::fs::rename(&tmp, path).await.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return;
    }
    tracing::info!("scrubbed AGENTGRID_ENROLL_TOKEN from {}", path.display());
}
