//! Artifact storage (bytes + metadata, path-safe). Extracted from `store.rs`.

use super::{
    is_safe_artifact_name, is_safe_opaque_id, now_iso, sha256_bytes_hex, Store,
    StoreArtifactError,
};
use agentgrid_common::{ArtifactMeta, ArtifactUploadResponse, UploadArtifactRequest};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

impl Store {
    // ----- artifacts (Stage 2.8) -----

    /// Persist an artifact's bytes on the control-plane filesystem and record
    /// its metadata. `content` is treated as UTF-8 text (patches/logs).
    /// Resolve `attempt_id/name` to an absolute path inside the artifact root,
    /// rejecting traversal. Canonicalizes the parent (created lazily) and checks
    /// the final name is a single safe segment so a symlinked worktree dir or a
    /// `..`-laden name cannot escape the root (Stage 2.2 defense-in-depth).
    pub(crate) fn artifact_path(&self, attempt_id: &str, name: &str) -> Result<std::path::PathBuf> {
        // Hardening P0: validate attempt_id as a safe opaque ID before it is
        // joined into a filesystem path. IDs are UUIDv4 (36 hex+hyphens) but we
        // accept any short `[A-Za-z0-9_-]` token so future ID schemes stay safe.
        // This is defense-in-depth: handler-level ownership checks already reject
        // unknown IDs, but a malformed ID must never reach a path join.
        if !is_safe_opaque_id(attempt_id) {
            anyhow::bail!("invalid attempt_id");
        }
        if !is_safe_artifact_name(name) {
            anyhow::bail!("invalid artifact name");
        }
        let dir = self.artifact_root.join(attempt_id);
        // Reject a symlinked attempt dir before any canonical check — a
        // symlink pointing outside the root would otherwise escape even with
        // a safe name.
        if let Ok(md) = std::fs::symlink_metadata(&dir) {
            if md.file_type().is_symlink() {
                anyhow::bail!("artifact dir is a symlink");
            }
        }
        // If the dir already exists, verify it is still inside the (canonical)
        // artifact root; if it does not exist yet (write path just created it
        // via create_dir_all, read path will 404 anyway) skip the canonical
        // dance — is_safe_opaque_id already guarantees `dir` is lexically
        // inside the root, so there is nothing to escape.
        let file_path = if let Ok(canon_dir) = dir.canonicalize() {
            let canon_root = self
                .artifact_root
                .canonicalize()
                .unwrap_or_else(|_| self.artifact_root.clone());
            if !canon_dir.starts_with(&canon_root) {
                anyhow::bail!("artifact dir escapes root");
            }
            canon_dir.join(name)
        } else {
            // dir does not exist yet — lexical join is safe (opaque id + safe
            // name cannot contain separators).
            dir.join(name)
        };
        // Hardening P0: the resolved file itself must not be a symlink.
        if let Ok(md) = std::fs::symlink_metadata(&file_path) {
            if md.file_type().is_symlink() {
                anyhow::bail!("artifact file is a symlink");
            }
        }
        Ok(file_path)
    }

    pub async fn save_artifact(
        &self,
        attempt_id: &str,
        req: &UploadArtifactRequest,
    ) -> Result<ArtifactUploadResponse, StoreArtifactError> {
        self.save_artifact_bytes(
            attempt_id,
            &req.name,
            req.content.as_bytes(),
            req.media_type.as_deref(),
            req.sha256.as_deref(),
        )
        .await
    }

    /// Stage 2.2 binary-safe artifact write: raw bytes + optional media type
    /// and hex SHA-256. Idempotent per (attempt_id, name). The legacy text
    /// endpoint forwards here with `content.as_bytes()`.
    pub async fn save_artifact_bytes(
        &self,
        attempt_id: &str,
        name: &str,
        bytes: &[u8],
        media_type: Option<&str>,
        sha256: Option<&str>,
    ) -> Result<ArtifactUploadResponse, StoreArtifactError> {
        // Hardening P0: validate attempt_id before any filesystem path join.
        if !is_safe_opaque_id(attempt_id) {
            return Err(StoreArtifactError::InvalidAttemptId);
        }
        // Hardening P0 (artifact integrity): always compute the server-side
        // SHA-256 of the uploaded bytes. If the caller supplied a sha256 hint
        // (JSON `sha256` field or raw `x-artifact-sha256` header) and it
        // disagrees, reject with `HashMismatch` (handler -> 422). We store
        // only the computed server-side hash, never the client value.
        let computed = sha256_bytes_hex(bytes);
        if let Some(expected) = sha256 {
            let expected = expected.trim().to_ascii_lowercase();
            if !expected.is_empty() && expected != computed {
                return Err(StoreArtifactError::HashMismatch { expected, computed });
            }
        }
        let dir = self.artifact_root.join(attempt_id);
        tokio::fs::create_dir_all(&dir).await?;
        let path = self.artifact_path(attempt_id, name)?;
        // Hardening P0 (crash safety): write to a sibling temp file then
        // atomic rename, so a crash between write and metadata commit cannot
        // leave a half-written published artifact. Same dir => same fs rename.
        let tmp = path.with_extension("tmp.upload");
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        let size = bytes.len() as i64;
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO artifacts (id, attempt_id, name, size_bytes, stored_at, media_type, sha256) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(attempt_id, name) DO UPDATE SET \
                size_bytes = excluded.size_bytes, \
                stored_at = excluded.stored_at, \
                media_type = excluded.media_type, \
                sha256 = excluded.sha256",
        )
        .bind(&id)
        .bind(attempt_id)
        .bind(name)
        .bind(size)
        .bind(&now)
        .bind(media_type)
        .bind(&computed)
        .execute(&self.pool)
        .await?;
        Ok(ArtifactUploadResponse {
            name: name.to_string(),
            size_bytes: size,
            media_type: media_type.map(|s| s.to_string()),
            sha256: computed,
        })
    }

    /// Read a stored artifact's metadata by task id + name (latest attempt).
    pub async fn read_artifact_meta(
        &self,
        task_id: &str,
        name: &str,
    ) -> Result<Option<ArtifactMeta>> {
        let Some(attempt_id) = self.latest_attempt_id(task_id).await? else {
            return Ok(None);
        };
        let row = sqlx::query(
            "SELECT size_bytes, media_type, sha256 FROM artifacts WHERE attempt_id = ? AND name = ?",
        )
        .bind(&attempt_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| ArtifactMeta {
            size_bytes: r.try_get::<i64, _>("size_bytes").unwrap_or(0),
            media_type: r.try_get::<Option<String>, _>("media_type").ok().flatten(),
            sha256: r.try_get::<Option<String>, _>("sha256").ok().flatten(),
        }))
    }

    /// Read a stored artifact's raw bytes by task id + name (latest attempt).
    pub async fn read_artifact_bytes(&self, task_id: &str, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(attempt_id) = self.latest_attempt_id(task_id).await? else {
            return Ok(None);
        };
        let path = match self.artifact_path(&attempt_id, name) {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        match tokio::fs::read(&path).await {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Resolve the latest attempt id for a task (artifacts are per-attempt).
    pub async fn latest_attempt_id(&self, task_id: &str) -> Result<Option<String>> {
        let row =
            sqlx::query("SELECT id FROM attempts WHERE task_id = ? ORDER BY number DESC LIMIT 1")
                .bind(task_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| r.try_get::<String, _>("id")).transpose()?)
    }

    /// Read a stored artifact's content by task id + name (latest attempt).
    pub async fn read_artifact(&self, task_id: &str, name: &str) -> Result<Option<String>> {
        let Some(attempt_id) = self.latest_attempt_id(task_id).await? else {
            return Ok(None);
        };
        let path = match self.artifact_path(&attempt_id, name) {
            Ok(p) => p,
            // Invalid/traversal name: treat as absent rather than erroring,
            // so a crafted request cannot distinguish a valid artifact.
            Err(_) => return Ok(None),
        };
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
