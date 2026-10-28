//! Artifact replication across HA data replicas.
//!
//! The SQLite snapshot carries only metadata; bytes move separately, keyed by
//! `attempt_id/<stored name>` and verified by the SHA-256 of the *stored* file
//! (a compressed log is hashed as its `.zst` frame). The leader pushes every
//! upload to its peers (`AGENTGRID_HA_ARTIFACT_SYNC=eager`) and a replica that
//! is missing a file pulls it from the leader/peers on the read path.

use super::config::ArtifactSync;
use super::transport;
use crate::AppState;

/// Leader: copy one just-uploaded artifact to every data peer. Best-effort —
/// the on-read pull repairs any replica that was down during the push.
pub async fn push_eager(state: &AppState, attempt_id: &str, name: &str) {
    let rt = &state.ha;
    if !rt.is_ha() || !rt.is_leader() || rt.config.artifact_sync != ArtifactSync::Eager {
        return;
    }
    let blob = match state.store.artifact_blob_meta(attempt_id, name).await {
        Ok(Some(b)) => b,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("HA artifact push: metadata read failed: {e}");
            return;
        }
    };
    let Some(sha) = blob.sha256.clone() else {
        return;
    };
    let bytes = match state
        .store
        .artifact_blob_bytes(attempt_id, &blob.stored_name)
        .await
    {
        Ok(Some(b)) => b,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("HA artifact push: file read failed: {e}");
            return;
        }
    };
    if let Some(s3) = &rt.s3 {
        if let Err(e) = s3.put_artifact(&sha, &bytes).await {
            tracing::warn!("HA artifact push to object storage failed: {e}");
        }
        return;
    }
    for peer in &rt.config.peers {
        if let Err(e) =
            transport::put_artifact(rt, &peer.url, attempt_id, &blob.stored_name, &sha, &bytes)
                .await
        {
            tracing::warn!(peer = %peer.id, "HA artifact push failed: {e}");
        }
    }
}

/// Any data replica: make sure the stored file for `task_id/name` exists
/// locally. A no-op outside HA, when metadata has no stored hash (legacy
/// rows), or when the file is already present. Pulled bytes are verified
/// against the database's stored hash before they are written.
pub async fn ensure_local(state: &AppState, task_id: &str, name: &str) {
    let rt = &state.ha;
    if !rt.is_ha() {
        return;
    }
    let Some(attempt) = state.store.latest_attempt_id(task_id).await.ok().flatten() else {
        return;
    };
    ensure_attempt_local(state, &attempt, name).await;
}

pub async fn ensure_attempt_local(state: &AppState, attempt_id: &str, name: &str) {
    let rt = &state.ha;
    let blob = match state.store.artifact_blob_meta(attempt_id, name).await {
        Ok(Some(b)) => b,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("HA artifact pull: metadata read failed: {e}");
            return;
        }
    };
    let Some(sha) = blob.sha256.clone() else {
        return;
    };
    match state
        .store
        .artifact_blob_present(attempt_id, &blob.stored_name)
        .await
    {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            tracing::warn!("HA artifact pull: local stat failed: {e}");
            return;
        }
    }
    if let Some(s3) = &rt.s3 {
        match s3.get_artifact(&sha).await {
            Ok(Some(bytes)) if agentgrid_common::sha256_hex(&bytes) == sha => {
                match state
                    .store
                    .write_artifact_blob(attempt_id, &blob.stored_name, &bytes, &sha)
                    .await
                {
                    Ok(()) => tracing::info!(
                        attempt = %attempt_id,
                        artifact = %name,
                        "pulled HA artifact from object storage"
                    ),
                    Err(e) => tracing::warn!("HA artifact pull: write failed: {e}"),
                }
            }
            Ok(Some(_)) => {
                tracing::warn!(attempt = %attempt_id, "HA artifact object hash mismatch")
            }
            Ok(None) => {
                tracing::warn!(attempt = %attempt_id, "HA artifact object missing in storage")
            }
            Err(e) => tracing::warn!("HA artifact pull from object storage failed: {e}"),
        }
        return;
    }
    let mut bases: Vec<String> = Vec::new();
    if let Some(leader) = rt.shared.leader() {
        if leader.id != rt.config.node_id && rt.config.peers.iter().any(|p| p.id == leader.id) {
            bases.push(leader.url);
        }
    }
    for peer in &rt.config.peers {
        if !bases.contains(&peer.url) {
            bases.push(peer.url.clone());
        }
    }
    for base in bases {
        match transport::get_artifact(rt, &base, attempt_id, &blob.stored_name).await {
            Ok((bytes, peer_sha)) if peer_sha == sha => {
                match state
                    .store
                    .write_artifact_blob(attempt_id, &blob.stored_name, &bytes, &sha)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            attempt = %attempt_id,
                            artifact = %name,
                            "pulled HA artifact from peer"
                        );
                        return;
                    }
                    Err(e) => tracing::warn!("HA artifact pull: write failed: {e}"),
                }
            }
            Ok(_) => tracing::warn!(base, "HA artifact pull: hash mismatch, trying next peer"),
            Err(e) => tracing::debug!(base, "HA artifact pull failed: {e}"),
        }
    }
    tracing::warn!(attempt = %attempt_id, artifact = %name, "HA artifact pull exhausted peers");
}
