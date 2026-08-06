//! Artifact routes: download (user + node) and upload (JSON + raw bytes).

use std::sync::Arc;

use agentgrid_common::UploadArtifactRequest;
use axum::{
    body::Bytes,
    extract::{Extension, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use crate::auth::{fencing_token_header, AuthedNode};
use crate::services::{ArtifactService, UploadArtifact};
use crate::AppState;

pub async fn get_artifact(
    State(state): State<Arc<AppState>>,
    Path((task_id, name)): Path<(String, String)>,
) -> Result<Response, StatusCode> {
    // Plan 535: name safety + read + metadata coordinated in ArtifactService.
    match ArtifactService::read(&state, &task_id, &name).await {
        Ok(Some((bytes, mt))) => Ok(artifact_response(
            bytes,
            mt.as_ref().and_then(|m| m.media_type.as_deref()),
            &name,
            mt.as_ref().and_then(|m| m.sha256.as_deref()),
        )),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => {
            tracing::error!("read_artifact failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

pub async fn get_artifact_node(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path((task_id, name)): Path<(String, String)>,
) -> Result<Response, StatusCode> {
    // Stage 8 / line 257: node-side mirror of `get_artifact` so the node can
    // fetch an upstream worker's `changes.patch` artifact with its own
    // node-credential (no user JWT available on the node). Plan 535: name
    // safety + producer authorization + read coordinated in ArtifactService.
    match ArtifactService::read_node(&state, &auth.node_id, &task_id, &name).await {
        Ok(Some((bytes, mt))) => Ok(artifact_response(
            bytes,
            mt.as_ref().and_then(|m| m.media_type.as_deref()),
            &name,
            mt.as_ref().and_then(|m| m.sha256.as_deref()),
        )),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => {
            tracing::error!("read_artifact (node) failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

pub async fn upload_artifact(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<UploadArtifactRequest>,
) -> Response {
    // Plan 535: name safety + ownership + fencing + quota + save are
    // coordinated in ArtifactService; the handler only maps the outcome.
    match ArtifactService::upload(
        &state,
        UploadArtifact {
            node_id: &auth.node_id,
            attempt_id: &attempt_id,
            fencing: fencing_token_header(&headers).as_deref(),
            name: &req.name,
            bytes: req.content.as_bytes(),
            media_type: req.media_type.as_deref(),
            sha256: req.sha256.as_deref(),
        },
    )
    .await
    {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => StatusCode::from(e).into_response(),
    }
}

/// Stage 2.2 binary-safe artifact upload: the request body is raw bytes (not
/// UTF-8 JSON), with the artifact name, optional media type, and optional hex
/// SHA-256 carried in headers. Idempotent per (attempt_id, name) on the store.
/// The node uses this for `changes.patch` (binary diffs) and any non-text
/// artifact; the legacy JSON endpoint stays for text-only clients.
pub async fn upload_artifact_raw(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path(attempt_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let name = match headers
        .get("x-artifact-name")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
    {
        Some(n) => n,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };
    let media_type = headers
        .get("x-artifact-media-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let sha256 = headers
        .get("x-artifact-sha256")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    // Plan 535: ownership + fencing + size + quota + save in ArtifactService.
    match ArtifactService::upload(
        &state,
        UploadArtifact {
            node_id: &auth.node_id,
            attempt_id: &attempt_id,
            fencing: fencing_token_header(&headers).as_deref(),
            name: &name,
            bytes: &body,
            media_type: media_type.as_deref(),
            sha256: sha256.as_deref(),
        },
    )
    .await
    {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => StatusCode::from(e).into_response(),
    }
}

/// Hardening P0 (stored XSS / download safety): build a `Response` for a
/// served artifact. A small allowlist of inline-safe media types may be
/// served with their stored `Content-Type`; everything else is forced to
/// `application/octet-stream`. HTML / SVG / JavaScript / XML and unknown
/// types are always sent as an **attachment** with `Content-Disposition`,
/// and every artifact response adds `X-Content-Type-Options: nosniff` so a
/// browser never sniffs a download as HTML/script. `name` is encoded as a
/// safe RFC 6266 `filename` (ASCII-only, control chars stripped).
fn artifact_response(
    bytes: Vec<u8>,
    media_type: Option<&str>,
    name: &str,
    sha256: Option<&str>,
) -> Response {
    const INLINE_SAFE: &[&str] = &[
        "application/octet-stream",
        "text/plain",
        "application/json",
        "application/zip",
        "application/gzip",
        "application/x-tar",
        "application/x-bzip2",
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/webp",
    ];
    const ACTIVE: &[&str] = &[
        "text/html",
        "text/xml",
        "application/xml",
        "application/xhtml+xml",
        "image/svg+xml",
        "application/javascript",
        "text/javascript",
        "application/ecmascript",
    ];
    let stored = media_type.unwrap_or("application/octet-stream").trim();
    let (content_type, attachment) = if ACTIVE.contains(&stored) {
        ("application/octet-stream", true)
    } else if INLINE_SAFE.contains(&stored) {
        (stored, false)
    } else {
        // Unknown type: never trust the client-requested type inline.
        ("application/octet-stream", true)
    };
    // ponytail: extension-based sniﬃng is a P2 follow-up; the allowlist +
    // nosniff + attachment triplet already blocks inline exﬆecution.
    let safe_name: String = name
        .chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control() && *c != '/' && *c != '\\' && *c != '"')
        .collect::<String>()
        .trim_start_matches('.')
        .to_string();
    let mut resp = (
        [
            (header::CONTENT_TYPE, content_type),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            ),
            // Hardening P0 item 3: artifacts are opaque data, never a document
            // context. `default-src 'none'` blocks any plugin/inline execution
            // even if a browser ignores nosniff; CORP same-origin keeps a
            // cross-origin page from reading artifact bytes.
            (
                header::HeaderName::from_static("content-security-policy"),
                "default-src 'none'; frame-ancestors 'none'",
            ),
            (
                header::HeaderName::from_static("cross-origin-resource-policy"),
                "same-origin",
            ),
        ],
        Bytes::from(bytes),
    )
        .into_response();
    if attachment {
        let cd = if safe_name.is_empty() {
            "attachment".to_string()
        } else {
            format!("attachment; filename=\"{}\"", safe_name)
        };
        if let Ok(v) = axum::http::HeaderValue::from_str(&cd) {
            resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
        }
    }
    // Hardening P2 item 36: expose the server-computed content hash so
    // clients (web UI) can show the artifact's integrity digest.
    if let Some(sha) = sha256 {
        if let Ok(v) = axum::http::HeaderValue::from_str(sha) {
            resp.headers_mut()
                .insert(header::HeaderName::from_static("x-artifact-sha256"), v);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::artifact_response;
    use axum::body::{to_bytes, Body};
    use axum::http::Response;

    fn hdr(resp: &Response<Body>, name: &str) -> Option<String> {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }

    #[tokio::test]
    async fn html_served_as_attachment_octetstream_with_nosniff() {
        let resp = artifact_response(b"<html></html>".to_vec(), Some("text/html"), "x.html", None);
        assert_eq!(
            hdr(&resp, "content-type").as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(
            hdr(&resp, "x-content-type-options").as_deref(),
            Some("nosniff")
        );
        let cd = hdr(&resp, "content-disposition").unwrap_or_default();
        assert!(cd.starts_with("attachment"), "cd={cd}");
        assert!(cd.contains("filename=\"x.html\""));
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    }

    #[tokio::test]
    async fn svg_served_as_attachment_octetstream_with_nosniff() {
        let resp = artifact_response(b"<svg/>".to_vec(), Some("image/svg+xml"), "logo.svg", None);
        assert_eq!(
            hdr(&resp, "content-type").as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(
            hdr(&resp, "x-content-type-options").as_deref(),
            Some("nosniff")
        );
    }

    #[tokio::test]
    async fn text_plain_inline() {
        let resp = artifact_response(b"hi".to_vec(), Some("text/plain"), "notes.txt", None);
        assert_eq!(hdr(&resp, "content-type").as_deref(), Some("text/plain"));
        assert!(hdr(&resp, "content-disposition").is_none());
    }

    #[tokio::test]
    async fn unknown_type_is_attachment() {
        let resp = artifact_response(
            b"x".to_vec(),
            Some("application/x-custom-weird"),
            "f.bin",
            None,
        );
        assert_eq!(
            hdr(&resp, "content-type").as_deref(),
            Some("application/octet-stream")
        );
        let cd = hdr(&resp, "content-disposition").unwrap_or_default();
        assert!(cd.starts_with("attachment"));
    }

    #[tokio::test]
    async fn dangerous_filename_sanitized() {
        let resp = artifact_response(b"x".to_vec(), Some("text/plain"), "../..\u{1}\"x", None);
        let cd = hdr(&resp, "content-disposition").unwrap_or_default();
        assert!(!cd.contains(".."), "cd={cd}");
        assert!(!cd.contains('\u{1}'), "cd={cd}");
    }

    #[tokio::test]
    async fn json_is_inline() {
        let resp = artifact_response(b"{}".to_vec(), Some("application/json"), "m.json", None);
        assert_eq!(
            hdr(&resp, "content-type").as_deref(),
            Some("application/json")
        );
    }
}
