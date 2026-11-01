//! Pull-based node join: the endpoints a host with nothing installed calls.
//!
//! Its own test binary because `AGENTGRID_NODE_DIST` is read from the process
//! environment and every test in one binary shares it — the same reason
//! `provision_credentials.rs` exists instead of living in `tests/api`.

use agentgrid_control_plane::{build_router, AppState};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static DIST: OnceLock<PathBuf> = OnceLock::new();

/// A stand-in for `AGENTGRID_NODE_DIST`, with two files that must be served and
/// three that must not.
fn dist() -> &'static Path {
    DIST.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("ag-join-dist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agentgrid-node-daemon"),
            b"#!/bin/sh\necho daemon\n",
        )
        .unwrap();
        std::fs::write(dir.join("adapter-mock"), b"#!/bin/sh\necho mock\n").unwrap();
        // Not in the allowlist: a dist dir is operator-chosen, and whatever
        // else lands there (a .env, a backup) is none of the joiner's business.
        std::fs::write(dir.join("secret.txt"), b"do not serve me").unwrap();
        std::fs::write(dir.join(".env"), b"TOKEN=leak").unwrap();
        std::fs::write(dir.join("notes.md"), b"# hi").unwrap();
        std::env::set_var("AGENTGRID_NODE_DIST", &dir);
        // Otherwise a developer's shell decides what the script says.
        std::env::remove_var("AGENTGRID_PUBLIC_URL");
        dir
    })
}

async fn boot() -> String {
    dist();
    let state = AppState::open_temp().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, build_router(state)).await;
    });
    format!("http://{addr}")
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The whole point of the endpoint: one line, with the control plane's address
/// already in it, so the operator never types a URL.
#[tokio::test]
async fn join_script_embeds_the_address_the_host_will_dial() {
    let base = boot().await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/v1/node/join.sh"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .starts_with("text/x-shellscript"),
        "curl | bash needs a script content type"
    );
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "a cached copy would keep pointing at an address the CP may have left"
    );
    let body = resp.text().await.unwrap();
    assert!(
        !body.contains("__AGENTGRID_SERVER__"),
        "the placeholder must be substituted before the script is served"
    );
    assert!(
        body.contains(&format!("SERVER=\"{base}\"")),
        "the script must default to the URL it was fetched from"
    );
    assert!(
        body.contains("--token"),
        "the token is the only argument it needs"
    );
    assert!(
        !body.contains("AGENTGRID_ENROLL_TOKEN="),
        "the script must never carry a token — the operator passes it in"
    );
}

#[tokio::test]
async fn the_installer_is_served_so_the_join_and_the_tarball_share_one_path() {
    let base = boot().await;
    let body = reqwest::Client::new()
        .get(format!("{base}/v1/node/install.sh"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("--staging"),
        "the served installer must be deploy/install-node.sh, which takes a staging dir"
    );
    assert!(
        body.contains("--data-dir"),
        "and the flag the join script passes"
    );
}

#[tokio::test]
async fn the_manifest_lists_the_daemon_and_adapters_only() {
    let base = boot().await;
    let body = reqwest::Client::new()
        .get(format!("{base}/v1/node/bundle"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let header = body.lines().next().unwrap_or_default();
    assert!(
        header.starts_with("# arch=") && header.contains(" os="),
        "the script refuses a wrong-arch bundle off this line, got {header:?}"
    );
    assert!(
        header.contains(std::env::consts::ARCH),
        "the CP's own arch: {header:?}"
    );

    let names: Vec<&str> = body
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| l.split(' ').next().unwrap())
        .collect();
    assert!(names.contains(&"agentgrid-node-daemon"), "{names:?}");
    assert!(names.contains(&"adapter-mock"), "{names:?}");
    for leak in ["secret.txt", ".env", "notes.md"] {
        assert!(
            !names.contains(&leak),
            "{leak} must not be in the bundle: {names:?}"
        );
    }

    // The digest in the manifest is the digest of the bytes on disk, which is
    // what the script verifies before installing anything.
    let daemon = std::fs::read(dist().join("agentgrid-node-daemon")).unwrap();
    let line = body
        .lines()
        .find(|l| l.starts_with("agentgrid-node-daemon "))
        .unwrap();
    let fields: Vec<&str> = line.split(' ').collect();
    assert_eq!(fields[1].parse::<usize>().unwrap(), daemon.len());
    assert_eq!(fields[2], sha256(&daemon));
}

#[tokio::test]
async fn a_bundle_file_carries_its_digest_in_a_header() {
    let base = boot().await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/v1/node/bundle/adapter-mock"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let header = resp
        .headers()
        .get("x-agentgrid-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(
        bytes.as_ref(),
        std::fs::read(dist().join("adapter-mock")).unwrap()
    );
    assert_eq!(
        header,
        sha256(&bytes),
        "the header must match the manifest line the script checked against"
    );
}

#[tokio::test]
async fn the_bundle_refuses_anything_outside_the_allowlist() {
    let base = boot().await;
    let http = reqwest::Client::new();
    for name in [
        "secret.txt",
        "notes.md",
        ".env",
        // Percent-encoded separators: axum decodes them, and the allowlist has
        // to reject what comes out the other side.
        "..%2F..%2Fetc%2Fpasswd",
        "%2e%2e%2fagentgrid-node-daemon",
        "agentgrid-node-daemon%2F..%2Fsecret.txt",
    ] {
        let resp = http
            .get(format!("{base}/v1/node/bundle/{name}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{name} must not be served");
    }
}

/// The exemption has to be exactly as wide as the join needs, no wider: every
/// other `/v1/node/` route still demands a credential.
#[tokio::test]
async fn the_rest_of_the_node_api_is_still_closed() {
    let base = boot().await;
    let http = reqwest::Client::new();
    let poll = http
        .post(format!("{base}/v1/node/poll"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        poll.status(),
        401,
        "an unauthenticated poll must still be rejected"
    );
    let heartbeat = http
        .post(format!("{base}/v1/node/heartbeat"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(heartbeat.status(), 401);
}
