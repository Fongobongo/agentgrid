//! Contract test: the OpenAPI document (`docs/openapi.yaml`) stays in sync
//! with the actual route table built in `crates/control-plane/src/lib.rs`.
//!
//! The OpenAPI document is the contract shared with the TypeScript web client
//! (web/src/api.ts). A route added in code without a doc entry, or a stale
//! doc entry, fails here so the two cannot drift.

use std::collections::BTreeSet;

/// Routes registered in `build_router` (extracted from the router builder in
/// lib.rs). Kept in sync by the test below.
fn code_routes() -> BTreeSet<String> {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"),
    )
    .expect("read lib.rs");
    src.lines()
        .filter_map(|l| {
            // Both `.route("/v1/...", ...)` and the multiline form
            // `"/v1/...",` appear in the router builder.
            let trimmed = l.trim();
            if let Some(rest) = trimmed.strip_prefix(".route(\"") {
                rest.split('"').next().map(|p| p.to_string())
            } else if trimmed.starts_with('"') && trimmed.contains("/v1") {
                trimmed.split('"').nth(1).map(|p| p.to_string())
            } else {
                None
            }
        })
        .filter(|p| p.starts_with('/'))
        .collect()
}

/// Paths in `docs/openapi.yaml`: top-level keys under `paths:` (each is a
/// two-space-indented line ending in `:`).
fn doc_paths() -> BTreeSet<String> {
    let yaml = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi.yaml"),
    )
    .expect("read docs/openapi.yaml");
    yaml.lines()
        .filter(|l| {
            // A path key line: two spaces, then `/`, no trailing handler text.
            l.starts_with("  /")
        })
        .filter_map(|l| {
            let path = l.trim().trim_end_matches(':');
            if path.starts_with('/') && !path.contains(' ') {
                Some(path.to_string())
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn every_code_route_is_documented() {
    let doc = doc_paths();
    let missing: Vec<String> = code_routes()
        .into_iter()
        .filter(|r| !doc.contains(r))
        .collect();
    assert!(
        missing.is_empty(),
        "routes registered in build_router but missing from docs/openapi.yaml: {missing:?}"
    );
}

#[test]
fn every_documented_route_exists_in_code() {
    let code = code_routes();
    let stale: Vec<String> = doc_paths()
        .into_iter()
        .filter(|p| !code.contains(p))
        .collect();
    assert!(
        stale.is_empty(),
        "routes in docs/openapi.yaml that no longer exist in build_router: {stale:?}"
    );
}

#[test]
fn openapi_version_is_3_1() {
    let yaml = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi.yaml"),
    )
    .expect("read docs/openapi.yaml");
    assert!(
        yaml.lines().any(|l| l.trim() == "openapi: 3.1.0"),
        "docs/openapi.yaml must declare `openapi: 3.1.0` (plan item 561)"
    );
}
