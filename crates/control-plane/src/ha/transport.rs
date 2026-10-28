//! Peer-to-peer HA transport. Every call carries the cluster token in a
//! header; redirects are disabled on the shared client so the token can never
//! be replayed to another host.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use super::protocol::{
    HaStatus, HeartbeatRequest, HeartbeatResponse, SnapshotManifest, VoteRequest, VoteResponse,
    ARTIFACT_ATTEMPT_HEADER, ARTIFACT_NAME_HEADER, ARTIFACT_SHA_HEADER, HA_TOKEN_HEADER,
};
use super::role::HaRuntime;

fn timeout(rt: &HaRuntime) -> Duration {
    (rt.config.election_timeout * 3).max(Duration::from_secs(2))
}

fn url(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim_end_matches('/'))
}

pub async fn vote(rt: &HaRuntime, base: &str, req: &VoteRequest) -> Result<VoteResponse> {
    let resp = rt
        .client()?
        .post(url(base, "/v1/ha/vote"))
        .header(HA_TOKEN_HEADER, &rt.config.cluster_token)
        .timeout(timeout(rt))
        .json(req)
        .send()
        .await
        .context("ha vote request")?;
    if !resp.status().is_success() {
        anyhow::bail!("ha vote rejected with HTTP {}", resp.status());
    }
    Ok(resp.json().await?)
}

pub async fn heartbeat(
    rt: &HaRuntime,
    base: &str,
    req: &HeartbeatRequest,
) -> Result<HeartbeatResponse> {
    let resp = rt
        .client()?
        .post(url(base, "/v1/ha/heartbeat"))
        .header(HA_TOKEN_HEADER, &rt.config.cluster_token)
        .timeout(timeout(rt))
        .json(req)
        .send()
        .await
        .context("ha heartbeat request")?;
    if !resp.status().is_success() {
        anyhow::bail!("ha heartbeat rejected with HTTP {}", resp.status());
    }
    Ok(resp.json().await?)
}

/// Push one stored artifact file to a peer. The body is the exact on-disk
/// bytes (compressed shape included); the peer verifies `sha256`.
pub async fn put_artifact(
    rt: &HaRuntime,
    base: &str,
    attempt_id: &str,
    stored_name: &str,
    sha256: &str,
    bytes: &[u8],
) -> Result<()> {
    let resp = rt
        .client()?
        .put(url(base, "/v1/ha/artifact"))
        .header(HA_TOKEN_HEADER, &rt.config.cluster_token)
        .header(ARTIFACT_ATTEMPT_HEADER, attempt_id)
        .header(ARTIFACT_NAME_HEADER, stored_name)
        .header(ARTIFACT_SHA_HEADER, sha256)
        .timeout(timeout(rt))
        .body(bytes.to_vec())
        .send()
        .await
        .context("ha artifact push")?;
    if !resp.status().is_success() {
        anyhow::bail!("ha artifact push returned HTTP {}", resp.status());
    }
    Ok(())
}

/// Fetch one stored artifact file from a peer. Returns the raw bytes and the
/// peer's advertised stored-byte hash (the caller verifies both).
pub async fn get_artifact(
    rt: &HaRuntime,
    base: &str,
    attempt_id: &str,
    stored_name: &str,
) -> Result<(Vec<u8>, String)> {
    let resp = rt
        .client()?
        .get(url(base, "/v1/ha/artifact"))
        .header(HA_TOKEN_HEADER, &rt.config.cluster_token)
        .header(ARTIFACT_ATTEMPT_HEADER, attempt_id)
        .header(ARTIFACT_NAME_HEADER, stored_name)
        .timeout(timeout(rt) * 4)
        .send()
        .await
        .context("ha artifact fetch")?;
    if !resp.status().is_success() {
        anyhow::bail!("ha artifact fetch returned HTTP {}", resp.status());
    }
    let sha = resp
        .headers()
        .get(ARTIFACT_SHA_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = resp.bytes().await?.to_vec();
    Ok((bytes, sha))
}

pub async fn status(rt: &HaRuntime, base: &str) -> Result<HaStatus> {
    let resp = rt
        .client()?
        .get(url(base, "/v1/ha/status"))
        .header(HA_TOKEN_HEADER, &rt.config.cluster_token)
        .timeout(timeout(rt))
        .send()
        .await
        .context("ha status request")?;
    if !resp.status().is_success() {
        anyhow::bail!("ha status returned HTTP {}", resp.status());
    }
    Ok(resp.json().await?)
}

pub async fn manifest(rt: &HaRuntime, base: &str, id: &str) -> Result<SnapshotManifest> {
    let resp = rt
        .client()?
        .get(url(base, &format!("/v1/ha/snapshot/{id}/manifest")))
        .header(HA_TOKEN_HEADER, &rt.config.cluster_token)
        .timeout(timeout(rt))
        .send()
        .await
        .context("ha snapshot manifest request")?;
    if !resp.status().is_success() {
        anyhow::bail!("ha snapshot manifest returned HTTP {}", resp.status());
    }
    Ok(resp.json().await?)
}

/// Stream a snapshot into `dest`; the caller verifies size and hash.
pub async fn download(rt: &HaRuntime, base: &str, id: &str, dest: &Path) -> Result<u64> {
    let mut resp = rt
        .client()?
        .get(url(base, &format!("/v1/ha/snapshot/{id}")))
        .header(HA_TOKEN_HEADER, &rt.config.cluster_token)
        .timeout(timeout(rt) * 20)
        .send()
        .await
        .context("ha snapshot download")?;
    if !resp.status().is_success() {
        anyhow::bail!("ha snapshot download returned HTTP {}", resp.status());
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let mut file = tokio::fs::File::create(dest).await?;
    let mut total = 0u64;
    while let Some(chunk) = resp.chunk().await? {
        total += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(total)
}
