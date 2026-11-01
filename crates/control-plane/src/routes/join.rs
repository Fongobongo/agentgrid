//! One-command node join — the pull-based twin of `ag nodes install`.
//!
//! `ag nodes install` (ADR 0027) pushes from the control plane over ssh, so it
//! needs credentials *to* the host and fails when the control plane cannot
//! reach it. This is the other direction: the host fetches what it needs from
//! the control plane it is joining, so the whole join is one line the operator
//! copies out of the panel and pastes into a shell on the new host:
//!
//! ```text
//! curl -fsSL https://cp.example.com/v1/node/join.sh | sudo bash -s -- --token <t>
//! ```
//!
//! Four endpoints, all deliberately unauthenticated — the enrollment token is
//! what authorizes a join, and it is single-use with a ten-minute TTL, which is
//! what makes a copy-paste command safe to hand out:
//!
//! - `GET /v1/node/join.sh` — the script, with the control plane's own public
//!   URL substituted in so the operator never types it.
//! - `GET /v1/node/install.sh` — the installer the script delegates to. Serving
//!   the same `deploy/install-node.sh` the release tarball ships keeps the join
//!   path and the tarball path on one implementation instead of two that drift.
//! - `GET /v1/node/bundle` — `<name> <size> <sha256>` per file, so the script
//!   can verify what it downloads without a JSON parser on the host.
//! - `GET /v1/node/bundle/{name}` — the bytes, with the same digest in an
//!   `x-agentgrid-sha256` header.
//!
//! The bundle is whatever `AGENTGRID_NODE_DIST` holds — the same directory
//! `ag nodes install` scp's from, so a control plane that can push can also
//! serve. Only the daemon and `adapter-*` files at the top level are exposed:
//! that directory is operator-chosen, and an endpoint that serves "whatever is
//! in this folder" is a file server for anything the control plane can read.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use crate::routes::provision::node_dist_dir;
use crate::AppState;

/// Placeholder in `deploy/join-node.sh`, replaced per request with the URL the
/// node should dial. Kept in the file rather than passed as an argument so the
/// script also works when read from the repo by a human.
const SERVER_PLACEHOLDER: &str = "__AGENTGRID_SERVER__";

const JOIN_SH: &str = include_str!("../../../../deploy/join-node.sh");
const INSTALL_SH: &str = include_str!("../../../../deploy/install-node.sh");

/// `GET /v1/node/join.sh`
pub async fn join_script(headers: HeaderMap) -> Response {
    let body = JOIN_SH.replace(SERVER_PLACEHOLDER, &public_url(&headers));
    script_response(body)
}

/// `GET /v1/node/install.sh`
pub async fn install_script() -> Response {
    script_response(INSTALL_SH.to_string())
}

fn script_response(body: String) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8"),
            // The body embeds this control plane's URL: a cached copy served
            // after a move would point the next host at the old address.
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

/// `GET /v1/node/bundle` — `<name> <size> <sha256>` per line.
///
/// The `# arch=` header line is what lets the script refuse to install an
/// x86-64 daemon on an arm host: these binaries were built for the control
/// plane's own platform, not for the joiner's.
pub async fn bundle_manifest(State(_state): State<Arc<AppState>>) -> Response {
    let Some(dir) = node_dist_dir() else {
        return plain(
            StatusCode::NOT_FOUND,
            "no node bundle: set AGENTGRID_NODE_DIST on the control plane to the \
             directory holding agentgrid-node-daemon",
        );
    };
    let mut body = format!(
        "# arch={} os={}\n",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    for (name, path) in shareable_files(&dir) {
        // A file that cannot be read is skipped rather than failing the whole
        // manifest: one unreadable adapter must not block joining with another.
        let Ok((size, sha)) = digest(&path) else {
            continue;
        };
        body.push_str(&format!("{name} {size} {sha}\n"));
    }
    if !body.lines().any(|l| l.starts_with("agentgrid-node-daemon")) {
        return plain(
            StatusCode::NOT_FOUND,
            "no node bundle: agentgrid-node-daemon is not in AGENTGRID_NODE_DIST",
        );
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

/// `GET /v1/node/bundle/{name}` — one file from the bundle.
pub async fn bundle_file(
    State(_state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    if !is_shareable(&name) {
        // Not "forbidden": a name outside the allowlist simply does not exist
        // here, and saying so keeps the allowlist out of the response.
        return plain(StatusCode::NOT_FOUND, "no such file in the node bundle");
    }
    let Some(dir) = node_dist_dir() else {
        return plain(StatusCode::NOT_FOUND, "no node bundle configured");
    };
    let path = dir.join(&name);
    // Resolve before reading: a symlink inside the dist dir would otherwise
    // turn this into a reader for any file the control plane can open.
    let Ok(real) = path.canonicalize() else {
        return plain(StatusCode::NOT_FOUND, "no such file in the node bundle");
    };
    let Ok(root) = dir.canonicalize() else {
        return plain(StatusCode::NOT_FOUND, "no node bundle configured");
    };
    if !real.starts_with(&root) || !real.is_file() {
        return plain(StatusCode::NOT_FOUND, "no such file in the node bundle");
    }
    let Ok(bytes) = tokio::fs::read(&real).await else {
        return plain(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not read the file from the node bundle",
        );
    };
    let sha = hex_digest(&bytes);
    // Built by hand rather than as a tuple: the digest is an owned String and
    // the tuple form needs every header value to be the same type.
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = StatusCode::OK;
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    // Hex cannot carry CR/LF, so this only fails if a future change to
    // `hex_digest` does something surprising — drop the header rather than
    // serve bytes whose digest the script cannot check.
    if let Ok(v) = header::HeaderValue::from_str(&sha) {
        headers.insert(header::HeaderName::from_static("x-agentgrid-sha256"), v);
    }
    resp
}

/// Files in `dir` that the bundle is allowed to expose.
fn shareable_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let path = e.path();
            // `is_file` follows symlinks; the containment check in
            // `bundle_file` is what actually stops an escape, this only keeps
            // directories out of the listing.
            if is_shareable(&name) && path.is_file() {
                Some((name, path))
            } else {
                None
            }
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The allowlist: the daemon and adapters, top level only, plain names.
///
/// `AGENTGRID_NODE_DIST` is a directory an operator points at; serving whatever
/// happens to be in it would make this endpoint a reader for any file the
/// control plane can open (a stray `.env`, a backup, a private key).
fn is_shareable(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 || name.starts_with('.') {
        return false;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return false;
    }
    name.starts_with("agentgrid-node-daemon") || name.starts_with("adapter-")
}

fn digest(path: &Path) -> std::io::Result<(u64, String)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((size, hex(hasher.finalize())))
}

fn hex_digest(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes))
}

fn hex(d: impl AsRef<[u8]>) -> String {
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// The URL the joining host should dial.
///
/// `AGENTGRID_PUBLIC_URL` wins when set: it is the operator's statement of
/// truth and survives a reverse proxy, a TLS terminator and a `Host` rewrite.
/// Otherwise the request's own `Host` is used, which is what makes the command
/// work with nothing configured.
fn public_url(headers: &HeaderMap) -> String {
    if let Ok(url) = std::env::var("AGENTGRID_PUBLIC_URL") {
        let url = url.trim().trim_end_matches('/');
        if !url.is_empty() {
            return url.to_string();
        }
    }
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| !h.is_empty())
        .unwrap_or("127.0.0.1:7800");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        // A proxy chain may send a list ("https, http"); the first entry is the
        // one the client used.
        .map(|p| p.split(',').next().unwrap_or(p).trim())
        .filter(|p| *p == "http" || *p == "https")
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

fn plain(code: StatusCode, msg: &str) -> Response {
    (
        code,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("{msg}\n"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shareable_names_are_the_daemon_and_adapters_only() {
        assert!(is_shareable("agentgrid-node-daemon"));
        assert!(is_shareable("agentgrid-node-daemon.exe"));
        assert!(is_shareable("adapter-mock"));
        assert!(is_shareable("adapter-claude.exe"));
        for bad in [
            "",
            "..",
            ".env",
            "config.json",
            "agentgrid-node-daemon/../../etc/passwd",
            "adapter-mock/../secrets",
            "notes.txt",
            &"a".repeat(65),
        ] {
            assert!(!is_shareable(bad), "{bad:?} must not be shareable");
        }
    }

    #[test]
    fn public_url_prefers_the_configured_value_then_the_host_header() {
        // `AGENTGRID_PUBLIC_URL` is process-global; save and restore so this
        // test cannot leak into the ones running beside it.
        let saved = std::env::var("AGENTGRID_PUBLIC_URL").ok();
        std::env::set_var("AGENTGRID_PUBLIC_URL", "https://cp.example.com/");
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "ignored:7800".parse().unwrap());
        assert_eq!(public_url(&h), "https://cp.example.com");

        std::env::remove_var("AGENTGRID_PUBLIC_URL");
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "10.0.0.5:7800".parse().unwrap());
        h.insert("x-forwarded-proto", "https, http".parse().unwrap());
        assert_eq!(public_url(&h), "https://10.0.0.5:7800");

        let mut h = HeaderMap::new();
        h.insert(header::HOST, "10.0.0.5:7800".parse().unwrap());
        h.insert("x-forwarded-proto", "javascript".parse().unwrap());
        assert_eq!(
            public_url(&h),
            "http://10.0.0.5:7800",
            "an unknown scheme falls back to http instead of being echoed"
        );

        match saved {
            Some(v) => std::env::set_var("AGENTGRID_PUBLIC_URL", v),
            None => std::env::remove_var("AGENTGRID_PUBLIC_URL"),
        }
    }

    #[test]
    fn the_script_carries_the_placeholder_and_the_installer_does_not() {
        assert!(
            JOIN_SH.contains(SERVER_PLACEHOLDER),
            "join.sh must be served with the control plane's URL substituted"
        );
        assert!(
            !INSTALL_SH.contains(SERVER_PLACEHOLDER),
            "install-node.sh takes its server as an argument, not a placeholder"
        );
    }
}
