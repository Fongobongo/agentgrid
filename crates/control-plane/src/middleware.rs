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
    // Do not overwrite if handler already set x-request-id (e.g. api_error_with_id).
    if !resp.headers().contains_key("x-request-id") {
        resp.headers_mut()
            .insert("x-request-id", id.as_str().try_into().unwrap());
    }
    resp
}

/// Hardening P2 item 19: a single, machine-readable JSON error envelope used
/// by handlers that already surface a typed status. `code` is a stable
/// snake_case string clients can switch on; the human message is short and
/// never includes internal error chains (those stay in structured logs).
/// The `X-Request-Id` header (added by the middleware) stays the correlation
/// key; it is also embedded in the body for clients that only read the body.
pub fn api_error_with_id(
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
    request_id: Option<&RequestId>,
) -> Response {
    let req_id = request_id
        .map(|r| r.0.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let body = serde_json::json!({
        "error": {
            "code": code,
            "message": message.into(),
            "request_id": req_id.clone(),
        }
    });
    let mut res = (status, Json(body)).into_response();
    // So request_id_middleware does not overwrite with a different id.
    let _ = res.headers_mut().try_insert(
        header::HeaderName::from_static("x-request-id"),
        header::HeaderValue::from_str(&req_id)
            .unwrap_or_else(|_| header::HeaderValue::from_static("")),
    );
    res
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
    // Hardening P0: reject symlinks that escape the web root. Walk each
    // prefix component for a symlink and canonical-check the file (or its
    // parent when the leaf does not exist) — a symlink dir would otherwise
    // escape even though the final canonicalize fails. Off the blocking pool.
    let web_root_c = web_root.clone();
    let rel_owned = rel.to_string();
    let forbidden = tokio::task::spawn_blocking(move || {
        let fs_path = web_root_c.join(&rel_owned);
        let mut cur = web_root_c.clone();
        for comp in std::path::Path::new(&rel_owned).components() {
            if let std::path::Component::Normal(os) = comp {
                cur = cur.join(os);
                if std::fs::symlink_metadata(&cur)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    return true;
                }
            }
        }
        if let Ok(canon) = fs_path.canonicalize() {
            if !canon.starts_with(&web_root_c) {
                return true;
            }
        } else if let Some(parent) = fs_path.parent() {
            if let Ok(canon_parent) = parent.canonicalize() {
                if !canon_parent.starts_with(&web_root_c) {
                    return true;
                }
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    if forbidden {
        return StatusCode::FORBIDDEN.into_response();
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
