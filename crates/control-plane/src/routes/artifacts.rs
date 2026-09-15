//! Artifact routes: download (user + node) and upload (JSON + raw bytes).

use std::sync::Arc;

use agentgrid_common::UploadArtifactRequest;
use axum::{
    body::{Body, Bytes},
    extract::{Extension, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use crate::auth::{fencing_token_header, AuthedNode};
use crate::services::{ArtifactService, UploadArtifact};
use crate::store::is_safe_artifact_name;
use crate::AppState;

/// Plan 1.11 (#8): list a task's artifacts (latest attempt) with metadata —
/// the SDK `artifacts()` surface. JSON array of `ArtifactMeta`.
pub async fn list_artifacts(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Json<Vec<agentgrid_common::ArtifactMeta>>, StatusCode> {
    state
        .store
        .list_artifacts(&task_id)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn get_artifact(
    State(state): State<Arc<AppState>>,
    Path((task_id, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    // Plan 6.7 (streaming): serve the artifact from disk without loading
    // it into RAM — a 1 GiB agent-raw-output.log used to materialize fully
    // in memory per request. `Range` is honored (206 partial + 416 on an
    // unsatisfiable start), matching the artifacts-API contract callers
    // already assume from any HTTP file server.
    // Plan 535: name safety — deny without disclosing existence.
    if !is_safe_artifact_name(&name) {
        return Err(StatusCode::NOT_FOUND);
    }
    let mt = state
        .store
        .read_artifact_meta(&task_id, &name)
        .await
        .ok()
        .flatten();
    let Some(open) = state
        .store
        .open_artifact(&task_id, &name)
        .await
        .map_err(|e| {
            tracing::error!("read_artifact failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
    else {
        return Err(StatusCode::NOT_FOUND);
    };
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    Ok(stream_artifact_response(
        open,
        mt.as_ref().and_then(|m| m.media_type.as_deref()),
        &name,
        mt.as_ref().and_then(|m| m.sha256.as_deref()),
        range.as_deref(),
    )
    .await)
}

pub async fn get_artifact_node(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Path((task_id, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    // Stage 8 / line 257: node-side mirror of `get_artifact` so the node can
    // fetch an upstream worker's `changes.patch` artifact with its own
    // node-credential (no user JWT available on the node). Plan 535: name
    // safety + producer authorization stay enforced; Plan 6.7: streamed.
    if !is_safe_artifact_name(&name) {
        return Err(StatusCode::NOT_FOUND);
    }
    let allowed = state
        .store
        .can_node_read_upstream_artifact(&auth.node_id, &task_id)
        .await
        .map_err(|e| {
            tracing::error!("read_artifact (node) failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if !allowed {
        return Err(StatusCode::NOT_FOUND);
    }
    let mt = state
        .store
        .read_artifact_meta(&task_id, &name)
        .await
        .ok()
        .flatten();
    let Some(open) = state
        .store
        .open_artifact(&task_id, &name)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::NOT_FOUND);
    };
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    Ok(stream_artifact_response(
        open,
        mt.as_ref().and_then(|m| m.media_type.as_deref()),
        &name,
        mt.as_ref().and_then(|m| m.sha256.as_deref()),
        range.as_deref(),
    )
    .await)
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

/// Plan 6.7 (streaming + zstd): build the artifact `Response` from an open
/// file handle, honoring a single-range `Range` header (`bytes=<start>-<end>`,
/// `bytes=<start>-`, `bytes=-<suffix>`) with 206/Content-Range, or 416 when
/// the range cannot be satisfied. The body is a bounded `ReaderStream` over
/// the (seeked) file — the content never lands in RAM whole, so a 1 GiB raw
/// log serves at flat memory cost. Multi-range requests fall back to a full
/// 200 stream (honest, simpler than interleaving multipart/byteranges for
/// a download API nothing consumes in slices).
///
/// A zstd-compressed backing file (Plan 6.7 log compression) streams
/// through an async decoder; `Content-Length` stays the logical uncompressed
/// length, and a Range header on a compressed body is ignored (full 200) —
/// a server may ignore a Range it cannot honor (RFC 9110 §14.2), and random
/// access into a zstd frame would require a seek-table we don't write.
///
/// Download-safety hardening (hardening P0) is unchanged from the previous
/// in-memory builder: active content types are forced to octet-stream +
/// attachment, everything else follows the inline allowlist, and every
/// response carries nosniff + CSP + CORP.
async fn stream_artifact_response(
    open: crate::store::OpenArtifact,
    media_type: Option<&str>,
    name: &str,
    sha256: Option<&str>,
    range_spec: Option<&str>,
) -> Response {
    use crate::store::OpenArtifact;
    let OpenArtifact {
        file,
        len,
        compressed,
    } = open;
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
    // ponytail: extension-based sniffing is a P2 follow-up; the allowlist +
    // nosniff + attachment triplet already blocks inline execution.
    let safe_name: String = name
        .chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control() && *c != '/' && *c != '\\' && *c != '"')
        .collect::<String>()
        .trim_start_matches('.')
        .to_string();

    // Range resolution (pure parser below): 0..serve_len from disk.
    // Compressed bodies ignore Range entirely (see doc).
    let (serve_from, serve_len, partial) = if compressed {
        (0u64, len, false)
    } else {
        match range_spec {
            Some(spec) => match parse_single_range(spec, len) {
                RangeVerdict::Full => (0, len, false),
                RangeVerdict::Partial { from, take } => (from, take, true),
                RangeVerdict::Unsatisfiable => {
                    // RFC 9110: 416 must carry Content-Range: bytes */<len>.
                    return Response::builder()
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                        .body(Body::empty())
                        .expect("static 416 response");
                }
            },
            None => (0, len, false),
        }
    };

    // Seek to the range start before any body byte is produced. A failure
    // serves an empty body (honest truncation) rather than the wrong bytes.
    let mut file = file;
    if serve_from > 0 {
        use tokio::io::AsyncSeekExt;
        if let Err(e) = file.seek(std::io::SeekFrom::Start(serve_from)).await {
            tracing::warn!("artifact seek failed: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    use tokio::io::AsyncReadExt;
    let body = if compressed {
        // Decode the zstd frame on the wire; the decoder streams, never
        // holding the full frame in RAM.
        let decoder =
            async_compression::tokio::bufread::ZstdDecoder::new(tokio::io::BufReader::new(file));
        Body::from_stream(tokio_util::io::ReaderStream::new(decoder))
    } else {
        let reader = tokio::io::BufReader::with_capacity(64 * 1024, file).take(serve_len);
        Body::from_stream(tokio_util::io::ReaderStream::new(reader))
    };

    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::HeaderName::from_static("x-content-type-options"),
            "nosniff",
        )
        // Hardening P0 item 3: artifacts are opaque data, never a document
        // context. `default-src 'none'` blocks any plugin/inline execution
        // even if a browser ignores nosniff; CORP same-origin keeps a
        // cross-origin page from reading artifact bytes.
        .header(
            header::HeaderName::from_static("content-security-policy"),
            "default-src 'none'; frame-ancestors 'none'",
        )
        .header(
            header::HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        )
        .header(header::CONTENT_LENGTH, serve_len.to_string())
        .header(
            header::ACCEPT_RANGES,
            header::HeaderValue::from_static("bytes"),
        );
    if partial {
        builder = builder.status(StatusCode::PARTIAL_CONTENT).header(
            header::CONTENT_RANGE,
            format!(
                "bytes {}-{}/{}",
                serve_from,
                serve_from + serve_len - 1,
                len
            ),
        );
    }
    if attachment {
        let cd = if safe_name.is_empty() {
            "attachment".to_string()
        } else {
            format!("attachment; filename=\"{}\"", safe_name)
        };
        builder = builder.header(header::CONTENT_DISPOSITION, cd);
    }
    // Hardening P2 item 36: expose the server-computed content hash so
    // clients (web UI) can show the artifact's integrity digest.
    if let Some(sha) = sha256 {
        builder = builder.header(header::HeaderName::from_static("x-artifact-sha256"), sha);
    }
    builder.body(body).expect("static artifact response")
}

/// Verdict of parsing a single `Range` header value against a known length.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RangeVerdict {
    /// Serve the whole file (no/ignorable range, malformed spec).
    Full,
    /// Serve `take` bytes starting at `from`.
    Partial { from: u64, take: u64 },
    /// The range cannot be satisfied (start past EOF, empty suffix).
    Unsatisfiable,
}

/// Pure single-range parser (Plan 6.7): accepts `bytes=<a>-<b>`,
/// `bytes=<a>-`, `bytes=-<n>` (suffix). A multi-range spec (`a-b,c-d`) is
/// NOT honored (the caller documented the full-200 fallback); a non-`bytes`
/// unit or garbage is treated as no range — servers may ignore a Range
/// header they cannot understand (RFC 9110 §14.2), and never serving fewer
/// bytes than requested keeps downloads correct.
fn parse_single_range(spec: &str, len: u64) -> RangeVerdict {
    let Some(rest) = spec.trim().strip_prefix("bytes=") else {
        return RangeVerdict::Full;
    };
    if rest.contains(',') {
        return RangeVerdict::Full; // multi-range: full-stream fallback
    }
    let Some((start, end)) = rest.split_once('-') else {
        return RangeVerdict::Full;
    };
    let (start, end) = (start.trim(), end.trim());
    if start.is_empty() {
        // Suffix range: last N bytes.
        let Some(n) = end.parse::<u64>().ok() else {
            return RangeVerdict::Full;
        };
        if n == 0 || len == 0 {
            return RangeVerdict::Unsatisfiable;
        }
        let take = n.min(len);
        RangeVerdict::Partial {
            from: len - take,
            take,
        }
    } else {
        let Some(from) = start.parse::<u64>().ok() else {
            return RangeVerdict::Full;
        };
        if from >= len {
            return RangeVerdict::Unsatisfiable;
        }
        let take = match end.parse::<u64>() {
            // Closed range: inclusive end, clamped to EOF.
            Ok(e) if e >= from => (e - from + 1).min(len - from),
            // Inverted (end < start) / open (`bytes=5-`) / garbage end:
            // serve from start to EOF — never fewer bytes than requested
            // for the portion that IS satisfiable, and an inverted spec is
            // ignorable (RFC 9110 lets a server ignore what it can't honor).
            _ => len - from,
        };
        RangeVerdict::Partial { from, take }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_single_range, stream_artifact_response, RangeVerdict};
    use axum::body::{to_bytes, Body};
    use axum::http::Response;

    fn hdr(resp: &Response<Body>, name: &str) -> Option<String> {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }

    /// Open + serve a temp file through the stream builder (Plan 6.7).
    /// `serve_shaped(.., true)` writes a zstd frame instead (the builder
    /// must decode it on the wire).
    async fn serve(
        content: &[u8],
        media_type: Option<&str>,
        name: &str,
        range: Option<&str>,
    ) -> Response<Body> {
        self::serve_shaped(content, media_type, name, range, false).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn serve_shaped(
        content: &[u8],
        media_type: Option<&str>,
        name: &str,
        range: Option<&str>,
        compress: bool,
    ) -> Response<Body> {
        let dir =
            std::env::temp_dir().join(format!("ag-artifact-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("a");
        let bytes = if compress {
            zstd::stream::encode_all(content, 3).unwrap()
        } else {
            content.to_vec()
        };
        tokio::fs::write(&path, bytes).await.unwrap();
        let file = tokio::fs::File::open(&path).await.unwrap();
        let open = crate::store::OpenArtifact {
            file,
            len: content.len() as u64,
            compressed: compress,
        };
        let resp = Box::pin(stream_artifact_response(
            open, media_type, name, None, range,
        ))
        .await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        resp
    }

    async fn body(resp: Response<Body>) -> Vec<u8> {
        to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    // ---- pure Range parser ----

    #[test]
    fn range_parser_closed_open_suffix_and_clamps() {
        // Closed range, inclusive end.
        assert_eq!(
            parse_single_range("bytes=2-5", 10),
            RangeVerdict::Partial { from: 2, take: 4 }
        );
        // End past EOF clamps to EOF.
        assert_eq!(
            parse_single_range("bytes=8-99", 10),
            RangeVerdict::Partial { from: 8, take: 2 }
        );
        // Open range: to EOF.
        assert_eq!(
            parse_single_range("bytes=7-", 10),
            RangeVerdict::Partial { from: 7, take: 3 }
        );
        // Suffix range: last N bytes.
        assert_eq!(
            parse_single_range("bytes=-4", 10),
            RangeVerdict::Partial { from: 6, take: 4 }
        );
        // Suffix larger than the file serves the whole file.
        assert_eq!(
            parse_single_range("bytes=-99", 10),
            RangeVerdict::Partial { from: 0, take: 10 }
        );
    }

    #[test]
    fn range_parser_unsatisfiable_and_ignorable() {
        assert_eq!(
            parse_single_range("bytes=10-", 10),
            RangeVerdict::Unsatisfiable
        );
        assert_eq!(
            parse_single_range("bytes=99-100", 10),
            RangeVerdict::Unsatisfiable
        );
        assert_eq!(
            parse_single_range("bytes=-0", 10),
            RangeVerdict::Unsatisfiable
        );
        // Non-bytes unit / garbage / multi-range: ignorable → full 200.
        assert_eq!(parse_single_range("items=1-2", 10), RangeVerdict::Full);
        assert_eq!(parse_single_range("bytes=garbage", 10), RangeVerdict::Full);
        assert_eq!(parse_single_range("bytes=0-1,5-6", 10), RangeVerdict::Full);
        // Inverted range (end < start): the parser serves from start to EOF
        // (ignorable tail), matching the "never fewer bytes than requested"
        // fallback — documented shape, asserted here.
        assert_eq!(
            parse_single_range("bytes=5-2", 10),
            RangeVerdict::Partial { from: 5, take: 5 }
        );
    }

    // ---- streaming serve (hardening triplet + Range semantics) ----

    #[tokio::test]
    async fn full_stream_serves_content_and_safety_headers() {
        let resp = serve(b"0123456789", Some("text/plain"), "log.txt", None).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(hdr(&resp, "accept-ranges").as_deref(), Some("bytes"));
        assert_eq!(hdr(&resp, "content-length").as_deref(), Some("10"));
        assert_eq!(hdr(&resp, "content-type").as_deref(), Some("text/plain"));
        assert_eq!(
            hdr(&resp, "x-content-type-options").as_deref(),
            Some("nosniff")
        );
        assert_eq!(body(resp).await, b"0123456789");
    }

    #[tokio::test]
    async fn range_request_serves_206_slice() {
        let resp = serve(
            b"0123456789",
            Some("text/plain"),
            "log.txt",
            Some("bytes=2-5"),
        )
        .await;
        assert_eq!(resp.status(), 206);
        assert_eq!(hdr(&resp, "content-range").as_deref(), Some("bytes 2-5/10"));
        assert_eq!(hdr(&resp, "content-length").as_deref(), Some("4"));
        assert_eq!(body(resp).await, b"2345");
    }

    #[tokio::test]
    async fn suffix_and_open_ranges() {
        let resp = serve(
            b"0123456789",
            Some("text/plain"),
            "log.txt",
            Some("bytes=-4"),
        )
        .await;
        assert_eq!(resp.status(), 206);
        assert_eq!(body(resp).await, b"6789");
        let resp = serve(
            b"0123456789",
            Some("text/plain"),
            "log.txt",
            Some("bytes=7-"),
        )
        .await;
        assert_eq!(resp.status(), 206);
        assert_eq!(body(resp).await, b"789");
    }

    #[tokio::test]
    async fn unsatisfiable_range_is_416() {
        let resp = serve(
            b"0123456789",
            Some("text/plain"),
            "log.txt",
            Some("bytes=10-"),
        )
        .await;
        assert_eq!(resp.status(), 416);
        assert_eq!(hdr(&resp, "content-range").as_deref(), Some("bytes */10"));
        assert!(body(resp).await.is_empty());
    }

    #[tokio::test]
    async fn html_served_as_attachment_octetstream_with_nosniff() {
        let resp = serve(b"<html></html>", Some("text/html"), "x.html", None).await;
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
        let _ = body(resp).await;
    }

    #[tokio::test]
    async fn svg_served_as_attachment_octetstream_with_nosniff() {
        let resp = serve(b"<svg/>", Some("image/svg+xml"), "logo.svg", None).await;
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
        let resp = serve(b"hi", Some("text/plain"), "notes.txt", None).await;
        assert_eq!(hdr(&resp, "content-type").as_deref(), Some("text/plain"));
        assert!(hdr(&resp, "content-disposition").is_none());
    }

    #[tokio::test]
    async fn unknown_type_is_attachment() {
        let resp = serve(b"x", Some("application/x-custom-weird"), "f.bin", None).await;
        assert_eq!(
            hdr(&resp, "content-type").as_deref(),
            Some("application/octet-stream")
        );
        let cd = hdr(&resp, "content-disposition").unwrap_or_default();
        assert!(cd.starts_with("attachment"));
    }

    #[tokio::test]
    async fn dangerous_filename_sanitized() {
        let resp = serve(b"x", Some("text/plain"), "../..\u{1}\"x", None).await;
        let cd = hdr(&resp, "content-disposition").unwrap_or_default();
        assert!(!cd.contains(".."), "cd={cd}");
        assert!(!cd.contains('\u{1}'), "cd={cd}");
    }

    #[tokio::test]
    async fn json_is_inline() {
        let resp = serve(b"{}", Some("application/json"), "m.json", None).await;
        assert_eq!(
            hdr(&resp, "content-type").as_deref(),
            Some("application/json")
        );
    }

    // ---- Plan 6.7: zstd-compressed backing files ----

    #[tokio::test]
    async fn compressed_artifact_decodes_on_the_wire() {
        // A zstd frame on disk must serve the ORIGINAL bytes with the
        // logical (uncompressed) Content-Length — the compression is an
        // on-disk detail invisible to API consumers.
        let content: Vec<u8> = "agent log line\n".repeat(600).into_bytes(); // ~8.4 KiB, compresses well
        let resp = serve_shaped(&content, Some("text/plain"), "big.log", None, true).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(
            hdr(&resp, "content-length").as_deref(),
            Some(content.len().to_string().as_str())
        );
        assert_eq!(body(resp).await, content);
    }

    #[tokio::test]
    async fn compressed_artifact_ignores_range() {
        // RFC 9110 §14.2: a server may ignore a Range it cannot honor —
        // random access into a zstd frame would need a seek-table we don't
        // write, so a compressed artifact answers a Range with the full
        // 200 representation, never fewer bytes than requested.
        let content: Vec<u8> = "0123456789".repeat(500).into_bytes();
        let resp = serve_shaped(
            &content,
            Some("text/plain"),
            "big.log",
            Some("bytes=0-9"),
            true,
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert!(hdr(&resp, "content-range").is_none());
        assert_eq!(body(resp).await, content);
    }

    #[test]
    fn compress_if_worthwhile_floor_and_win() {
        // Highly repetitive log: compresses → Some (smaller frame).
        let log: Vec<u8> = "spam spam spam\n".repeat(1000).into_bytes();
        let z = crate::store::compress_if_worthwhile_pub("a.log", &log);
        assert!(z.is_some());
        let z = z.unwrap();
        assert!(z.len() < log.len());
        // The frame round-trips.
        assert_eq!(zstd::stream::decode_all(z.as_slice()).unwrap(), log);
        // Incompressible content (high-entropy noise) → None (kept plain).
        let mut x: u32 = 0x9E3779B9;
        let noise: Vec<u8> = (0..4096u32)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        assert!(crate::store::compress_if_worthwhile_pub("a.log", &noise).is_none());
    }
}
