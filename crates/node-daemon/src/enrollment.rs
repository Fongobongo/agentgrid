//! Node enrollment: credential persistence, token scrubbing.

use anyhow::Result;

use crate::config::{adapter_permission_interception, Config, SavedCredential};
use agentgrid_common::{EnrollRequest, EnrollResponse};

/// Load saved credential or enroll a new node. Returns the credential for
/// subsequent heartbeats/polls.
pub async fn load_or_enroll(cfg: &Config) -> Result<SavedCredential> {
    let saved = std::fs::read_to_string(&cfg.credential_path)
        .ok()
        .and_then(|data| serde_json::from_str::<SavedCredential>(&data).ok());

    // ADR 0028 re-key: a re-key token plus `AGENTGRID_REKEY=1` replaces the
    // credential while keeping the identity, and — the point of the exercise —
    // the sealing key, so the ciphertexts the control plane holds for this node
    // still open and no adapter-env value has to be re-entered.
    //
    // Deliberately explicit. Re-enrolling on a plain restart would trade a
    // working credential for a one-shot token that may already be spent, and a
    // daemon that merely started twice would be locked out of its own node.
    if cfg.rekey {
        if cfg.enroll_token.is_none() {
            anyhow::bail!(
                "AGENTGRID_REKEY=1 needs a re-key token in AGENTGRID_ENROLL_TOKEN \
                 (mint one with `ag nodes rekey <node-id>`)"
            );
        }
        // The old file is not touched until the new credential is in hand: a
        // failed re-key (expired token, control plane unreachable) must leave
        // the node exactly as it was, with a credential that still works.
        let keep_key = saved.as_ref().and_then(|c| c.key_secret.clone());
        let cred = enroll_node(cfg, keep_key).await?;
        write_credential(&cfg.credential_path, &cred)?;
        scrub_enroll_token_from_env(cfg).await;
        tracing::info!(
            node = %cred.node_id,
            "re-keyed: same node id, same sealing key — sealed secrets survive"
        );
        return Ok(cred);
    }

    if let Some(mut cred) = saved {
        // ADR 0028: a credential written before sealing has no key. Mint
        // one now and save it, so the next heartbeat can announce it —
        // without this, an upgraded node silently never receives a
        // CP-managed secret again.
        if cred.key_secret.is_none() {
            let (_, private) = agentgrid_common::seal::generate_keypair()
                .map_err(|e| anyhow::anyhow!("generate node key: {e}"))?;
            cred.key_secret = Some(private);
            write_credential(&cfg.credential_path, &cred)?;
            tracing::info!(
                "generated an adapter-env key for this node; secrets sealed before this \
                 point must be re-entered with `ag adapter-env set`"
            );
        }
        return Ok(cred);
    }
    let cred = enroll_node(cfg, None).await?;
    write_credential(&cfg.credential_path, &cred)?;
    scrub_enroll_token_from_env(cfg).await;
    Ok(cred)
}

/// Persist the credential (and the sealing key) with 0600.
///
/// Node credential is secret material: 0600 regardless of umask (matches the
/// env-file scrub path), including for a pre-existing file. (0600 is a POSIX
/// concept; on Windows the best effort is a normal private write — ACL
/// hardening is out of scope for a dev host.)
fn write_credential(path: &std::path::Path, cred: &SavedCredential) -> Result<()> {
    let data = serde_json::to_vec(cred)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(&data)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, &data)?;
    Ok(())
}

/// The keypair to announce at enrollment: the one already on disk, or a fresh
/// one.
///
/// Split out from [`enroll_node`] because the reuse rule is the one thing a
/// re-key must not get wrong, and it is invisible at runtime: a fresh keypair
/// enrolls fine, the node works, and the only symptom is that the control plane
/// dropped every sealed adapter-env value for it.
fn sealing_key(keep: Option<String>) -> Result<(String, String)> {
    match keep {
        Some(secret) => {
            let public = agentgrid_common::seal::public_key_from_secret(&secret)
                .map_err(|e| anyhow::anyhow!("the saved sealing key is unreadable: {e}"))?;
            Ok((public, secret))
        }
        None => agentgrid_common::seal::generate_keypair()
            .map_err(|e| anyhow::anyhow!("generate node key: {e}")),
    }
}

/// Enroll this node with the control plane.
///
/// `keep_key` is the sealing key already on disk, if any. A re-key must reuse
/// it: the control plane keeps one ciphertext per node sealed to that key, and
/// a fresh one would make `set_node_public_key` drop them — losing every
/// adapter-env secret, which is precisely what a re-key exists to avoid. A
/// fresh enrollment passes `None` and mints a keypair.
async fn enroll_node(cfg: &Config, keep_key: Option<String>) -> Result<SavedCredential> {
    let client = crate::polling::daemon_http_client()?;
    // The public half is part of enrollment (ADR 0028), so a node is
    // addressable by the CP from the first moment it exists.
    let (public_key, key_secret) = sealing_key(keep_key)?;
    let body = EnrollRequest {
        public_key: Some(public_key),
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
                    key_secret: Some(key_secret),
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
                // A gateway error (502/504) is the one other status worth
                // another attempt: the endpoint answered, so it is up, but it
                // cannot reach the control plane — a relay whose own route
                // died, or a proxy in front of a dead one. The next configured
                // endpoint may well work, and this loop is bounded (two passes
                // over the list) and runs once at boot, so there is no
                // thrashing to avoid: one 502 is enough to move on. Any other
                // status is a real answer (401 bad token, 400 bad request) and
                // must not be retried.
                if status != reqwest::StatusCode::SERVICE_UNAVAILABLE
                    && !crate::config::is_gateway_status(status.as_u16())
                {
                    break;
                }
                tracing::warn!(
                    "enroll: control plane answered {status}; retrying at {}",
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

/// Remove any `AGENTGRID_ENROLL_TOKEN=...` or `AGENTGRID_REKEY=...` line from
/// `path` in place (atomic temp+rename, perm 0600). No-op if the file is
/// missing; logged failures are never fatal (the credential is already
/// persisted).
///
/// `AGENTGRID_REKEY` goes too, not just the token: left behind in an env file it
/// re-arms the re-key path on the next boot, and a spent token fails the boot —
/// a node that restarts once would never start again.
pub async fn scrub_token_from_file(path: &std::path::Path) {
    let Ok(text) = tokio::fs::read_to_string(path).await else {
        return;
    };
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("AGENTGRID_ENROLL_TOKEN=") && !t.starts_with("AGENTGRID_REKEY=")
        })
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
    tracing::info!(
        "scrubbed the one-shot enrollment variables from {}",
        path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole re-key contract in one assertion: the key already on disk is
    /// the key that gets announced. A regenerated one enrolls perfectly well and
    /// only shows up later, as an empty adapter env on a node nobody restarted.
    #[test]
    fn a_rekey_announces_the_key_that_is_already_on_disk() {
        let (public, secret) = agentgrid_common::seal::generate_keypair().unwrap();
        let (kept_public, kept_secret) = sealing_key(Some(secret.clone())).unwrap();
        assert_eq!(
            kept_secret, secret,
            "the private key must be reused verbatim"
        );
        assert_eq!(
            kept_public, public,
            "and the public half derived from it must be the same one"
        );
    }

    #[test]
    fn a_fresh_enroll_mints_a_key() {
        let (public_a, secret_a) = sealing_key(None).unwrap();
        let (public_b, _) = sealing_key(None).unwrap();
        assert_ne!(public_a, public_b, "two enrollments must not share a key");
        assert_eq!(
            agentgrid_common::seal::public_key_from_secret(&secret_a).unwrap(),
            public_a
        );
    }

    /// `AGENTGRID_REKEY` left behind in an env file re-arms the re-key path on
    /// the next boot, and a spent token fails the boot — a node that restarts
    /// once would never start again. So the scrub has to take the flag with the
    /// token, and leave everything else alone.
    #[tokio::test]
    async fn the_scrub_removes_the_rekey_flag_with_the_token() {
        let p = std::env::temp_dir().join(format!("ag-scrub-{}.env", uuid::Uuid::new_v4()));
        tokio::fs::write(
            &p,
            "AGENTGRID_ENROLL_TOKEN=secret-token\nAGENTGRID_REKEY=1\nAGENTGRID_SERVER=http://cp:7800\n",
        )
        .await
        .unwrap();

        scrub_token_from_file(&p).await;

        let left = tokio::fs::read_to_string(&p).await.unwrap();
        assert!(!left.contains("secret-token"), "the token survived: {left}");
        assert!(
            !left.contains("AGENTGRID_REKEY"),
            "the re-arm survived: {left}"
        );
        assert!(
            left.contains("AGENTGRID_SERVER=http://cp:7800"),
            "the scrub ate an unrelated line: {left}"
        );
        let _ = tokio::fs::remove_file(&p).await;
    }
}
