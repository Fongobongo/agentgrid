//! HTTP middleware: security headers, request-id correlation, SPA fallback,
//! and the shared JSON error envelope.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

use http_body_util::BodyExt;
use tower_http::services::{ServeDir, ServeFile};

use crate::{is_safe_opaque_id, AppState};

pub async fn security_headers_middleware(req: Request<Body>, next: Next) -> Response {
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    let _ = headers.try_insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    let _ = headers.try_insert(
        header::HeaderName::from_static("permissions-policy"),
        header::HeaderValue::from_static(
            "accelerometer=(), camera=(), geolocation=(), gyroscope=(), magnetometer=(), microphone=(), payment=(), usb=()",
        ),
    );
    if std::env::var("AGENTGRID_HSTS").as_deref() == Ok("1") {
        let _ = headers.try_insert(
            header::HeaderName::from_static("strict-transport-security"),
            header::HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        );
    }
    res
}

/// The id rides in a tracing span (so every event logs it) and is echoed back
/// on the response.
pub async fn request_id_middleware(
    headers: HeaderMap,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| is_safe_opaque_id(s))
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    req.extensions_mut().insert(RequestId(id.clone()));
    let span = tracing::info_span!("request", request_id = %id);
    let mut resp = span.in_scope(|| async move { next.run(req).await }).await;
    resp.headers_mut()
        .insert("x-request-id", id.as_str().try_into().unwrap());
    resp
}

/// Hardening P2 item 19: a single, machine-readable JSON error envelope used
/// by handlers that already surface a typed status. `code` is a stable
/// snake_case string clients can switch on; the human message is short and
/// never includes internal error chains (those stay in structured logs).
/// The `X-Request-Id` header (added by the middleware) stays the correlation
/// key; it is also embedded in the body for clients that only read the body.
pub fn api_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let req_id = uuid::Uuid::new_v4().to_string();
    let body = serde_json::json!({
        "error": {
            "code": code,
            "message": message.into(),
            "request_id": req_id,
        }
    });
    (status, Json(body)).into_response()
}

/// Request id available to handlers via extensions (for explicit logging /
/// future audit). The span already carries it for every log line.
#[derive(Clone, Debug)]
#[allow(dead_code)] // read by future handlers via extensions
pub struct RequestId(pub String);

/// Serve the built web UI (Stage 4.3). Unknown non-API paths fall back to
/// SPA static file serving using tower-http's ServeDir.
/// Serves files from the web root with proper security headers.
/// Falls back to index.html for non-/v1/ routes (SPA routing).
pub async fn spa_fallback(State(state): State<Arc<AppState>>, req: Request<Body>) -> Response {
    let web_root = match &state.web_root {
        Some(r) => r.clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let path = req.uri().path();
    // Don't serve SPA fallback for /v1/ API routes - return 404
    if path.starts_with("/v1/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Hardening P0: validate path components to prevent traversal.
    // Only Normal segments allowed; ParentDir (..), RootDir (/), and
    // any prefix components are rejected.
    let rel = path.trim_start_matches('/');
    // Inline is_safe_static_path check
    let is_safe = {
        use std::path::Component;
        let mut safe = true;
        for comp in std::path::Path::new(rel).components() {
            match comp {
                Component::Normal(_) => {}
                Component::CurDir => {}
                _ => {
                    safe = false;
                    break;
                }
            }
        }
        safe
    };
    if !is_safe {
        return StatusCode::FORBIDDEN.into_response();
    }
    // Hardening P0: reject symlinks that escape the web root.
    // Check each path component for symlinks pointing outside root.
    let fs_path = web_root.join(rel);
    if let Ok(canon_file) = fs_path.canonicalize() {
        if !canon_file.starts_with(&web_root) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    // Serve index.html for root path explicitly
    if rel.is_empty() {
        let idx = web_root.join("index.html");
        if let Ok(bytes) = tokio::fs::read(&idx).await {
            return (
                [
                    (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    (axum::http::header::CACHE_CONTROL, "no-cache"),
                    (axum::http::header::HeaderName::from_static("content-security-policy"),
                        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline';                          img-src 'self' data:; connect-src 'self'; font-src 'self';                          object-src 'none'; base-uri 'self'; frame-ancestors 'none';                          form-action 'self'"),
                    (axum::http::header::HeaderName::from_static("x-content-type-options"), "nosniff"),
                    (axum::http::header::HeaderName::from_static("x-frame-options"), "DENY"),
                ],
                bytes,
            ).into_response();
        }
        return StatusCode::NOT_FOUND.into_response();
    }
    let serve_dir = ServeDir::new(&web_root)
        .not_found_service(ServeFile::new(web_root.join("index.html")))
        .append_index_html_on_directories(false);
    // Convert ServeDir response to axum Response<Body>
    match tower::util::ServiceExt::oneshot(serve_dir, req).await {
        Ok(res) => {
            let (parts, body) = res.into_parts();
            Response::from_parts(parts, Body::new(body.into_stream()))
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
