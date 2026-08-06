//! Node enrollment: credential persistence, token scrubbing.

use anyhow::Result;
use reqwest::Client;

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
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let data = serde_json::to_vec(&cred)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&cfg.credential_path)?;
    f.write_all(&data)?;
    std::fs::set_permissions(&cfg.credential_path, std::fs::Permissions::from_mode(0o600))?;
    scrub_enroll_token_from_env(cfg).await;
    Ok(cred)
}

/// Enroll this node with the control plane.
async fn enroll_node(cfg: &Config) -> Result<SavedCredential> {
    let client = Client::new();
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
    let resp = client
        .post(format!("{}/v1/nodes/enroll", cfg.server))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let err = resp.text().await.unwrap_or_default();
        anyhow::bail!("enroll failed: {}", err);
    }
    let enrolled: EnrollResponse = resp.json().await?;
    Ok(SavedCredential {
        node_id: enrolled.node_id,
        credential: enrolled.credential,
    })
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
    use std::os::unix::fs::PermissionsExt;
    let _ = tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await;
    if tokio::fs::rename(&tmp, path).await.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return;
    }
    tracing::info!("scrubbed AGENTGRID_ENROLL_TOKEN from {}", path.display());
}
