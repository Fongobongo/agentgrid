//! Contract test: the OpenAPI document (`docs/openapi.yaml`) stays in sync
//! with the actual route table built in `crates/control-plane/src/lib.rs`
//! and with the DTOs the handlers actually serialise.
//!
//! The OpenAPI document is the contract shared with the TypeScript web client
//! (web/src/api.ts). A route added in code without a doc entry, or a stale
//! doc entry, fails here so the two cannot drift. The same applies to the two
//! DTOs a client actually posts and parses (`CreateTaskRequest`, `TaskView`):
//! they are checked field-by-field against the Rust structs in
//! `crates/common/src/lib.rs`, because a schema that is merely *absent* is a
//! silent hole — `$ref` resolution is checked separately.

use std::collections::BTreeSet;

use serde_json::Value;

fn repo_file(rel: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    // The tree is checked out with CRLF on Windows, and the parsers below
    // anchor on "\n"-prefixed markers (`\n    def run(`, `\n}`). Normalising
    // here keeps every call site line-ending agnostic.
    raw.replace("\r\n", "\n")
}

/// The OpenAPI document, parsed. Parsing (rather than line-scanning) is itself
/// part of the contract: a YAML syntax error used to sail through these tests.
fn openapi_doc() -> Value {
    let raw = repo_file("docs/openapi.yaml");
    serde_saphyr::from_str(&raw).expect("docs/openapi.yaml must parse as YAML")
}

/// Routes registered in `build_router` (extracted from the router builder in
/// lib.rs). Kept in sync by the test below.
fn code_routes() -> BTreeSet<String> {
    repo_file("crates/control-plane/src/lib.rs")
        .lines()
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

fn doc_paths(doc: &Value) -> BTreeSet<String> {
    doc["paths"]
        .as_object()
        .expect("docs/openapi.yaml must have a `paths:` map")
        .keys()
        .cloned()
        .collect()
}

/// Every `$ref` string anywhere in the document.
fn collect_refs(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if k == "$ref" {
                    if let Some(s) = val.as_str() {
                        out.insert(s.to_string());
                    }
                }
                collect_refs(val, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|x| collect_refs(x, out)),
        _ => {}
    }
}

/// Scheme names named by any `security:` requirement (an array of maps whose
/// keys are scheme names).
fn collect_security_names(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if k == "security" {
                    if let Some(reqs) = val.as_array() {
                        for req in reqs {
                            if let Some(m) = req.as_object() {
                                out.extend(m.keys().cloned());
                            }
                        }
                    }
                }
                collect_security_names(val, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|x| collect_security_names(x, out)),
        _ => {}
    }
}

fn names_in(doc: &Value, section: &str) -> BTreeSet<String> {
    doc["components"][section]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

#[test]
fn every_code_route_is_documented() {
    let doc = doc_paths(&openapi_doc());
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
    let stale: Vec<String> = doc_paths(&openapi_doc())
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
    let doc = openapi_doc();
    assert_eq!(
        doc["openapi"].as_str(),
        Some("3.1.0"),
        "docs/openapi.yaml must declare `openapi: 3.1.0` (plan item 561)"
    );
}

/// `version = "x"` under `[workspace.package]` in `Cargo.toml` — the one place
/// a release bumps.
fn workspace_version() -> String {
    let cargo = repo_file("Cargo.toml");
    let mut in_section = false;
    for line in cargo.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == "[workspace.package]";
            continue;
        }
        if in_section {
            if let Some(v) = line.strip_prefix("version") {
                let v = v.trim_start().trim_start_matches('=').trim();
                return v.trim_matches('"').to_string();
            }
        }
    }
    panic!("Cargo.toml must define [workspace.package] version");
}

/// `info.version` is the API version a generated client stamps into its own
/// metadata, so it has to track the release the document was written against.
/// It sat at `0.1.0` through four minor releases because nothing read it.
#[test]
fn info_version_matches_the_workspace_crate_version() {
    let doc = openapi_doc();
    let workspace_version = workspace_version();
    assert_eq!(
        doc["info"]["version"].as_str(),
        Some(workspace_version.as_str()),
        "docs/openapi.yaml `info.version` must equal the workspace crate version \
         ({workspace_version}); bump both together"
    );
}

/// Every `$ref: "#/components/<section>/<Name>"` must land on a definition.
///
/// This is what makes the document usable by a client generator: an
/// unresolvable `$ref` is not a cosmetic wart, it makes the whole schema
/// graph unrenderable in Swagger UI and ungeneratable in `openapi-typescript`.
#[test]
fn every_component_ref_resolves() {
    let doc = openapi_doc();
    let schemas = names_in(&doc, "schemas");
    let responses = names_in(&doc, "responses");
    let mut refs = BTreeSet::new();
    collect_refs(&doc, &mut refs);

    let mut dangling = Vec::new();
    for r in &refs {
        let Some(rest) = r.strip_prefix("#/components/") else {
            // External file refs are not used by this document; flag them so a
            // half-migrated include cannot slip in unnoticed.
            dangling.push(format!("{r} (only local #/components refs are supported)"));
            continue;
        };
        let Some((section, name)) = rest.split_once('/') else {
            dangling.push(format!(
                "{r} (malformed: expected #/components/<section>/<Name>)"
            ));
            continue;
        };
        let known = match section {
            "schemas" => &schemas,
            "responses" => &responses,
            _ => {
                dangling.push(format!("{r} (unknown components section `{section}`)"));
                continue;
            }
        };
        if !known.contains(name) {
            dangling.push(format!("{r} (no `{name}` under components.{section})"));
        }
    }
    assert!(
        dangling.is_empty(),
        "unresolvable $refs in docs/openapi.yaml: {dangling:#?}"
    );
}

/// Every scheme named in a `security:` requirement must exist under
/// `components.securitySchemes`, otherwise the requirement is unenforceable
/// for a generated client.
#[test]
fn every_security_requirement_names_a_defined_scheme() {
    let doc = openapi_doc();
    let defined = names_in(&doc, "securitySchemes");
    assert!(
        !defined.is_empty(),
        "docs/openapi.yaml defines no securitySchemes"
    );
    let mut used = BTreeSet::new();
    collect_security_names(&doc, &mut used);
    let unknown: Vec<&String> = used.iter().filter(|s| !defined.contains(*s)).collect();
    assert!(
        unknown.is_empty(),
        "security requirements name undefined schemes: {unknown:?} (defined: {defined:?})"
    );
}

/// Effective security scheme names for one operation: its own `security`
/// requirement, else the document-level default. An empty result means "no
/// credential" (an explicit `security: []`).
fn effective_schemes(doc: &Value, op: &Value) -> BTreeSet<String> {
    op.get("security")
        .or_else(|| doc.get("security"))
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_object())
                .flat_map(|m| m.keys().cloned())
                .collect()
        })
        .unwrap_or_default()
}

/// `/v1/node/*` is authenticated by the node credential, except `enroll`,
/// which issues it (see `auth::require_node_auth`). Documenting a node route
/// under the user scheme makes a generated client send a JWT to a route that
/// rejects it — and documenting a user route under the node scheme does the
/// same in reverse.
#[test]
fn node_routes_use_the_node_scheme() {
    let doc = openapi_doc();
    let mut wrong = Vec::new();
    for (path, item) in doc["paths"].as_object().expect("paths").iter() {
        if !path.starts_with("/v1/node/") {
            continue;
        }
        let enroll = path == "/v1/node/enroll";
        for (method, op) in item.as_object().expect("path item").iter() {
            if !matches!(method.as_str(), "get" | "post" | "put" | "delete" | "patch") {
                continue;
            }
            let schemes = effective_schemes(&doc, op);
            let ok = if enroll {
                schemes.is_empty()
            } else {
                schemes.contains("nodeAuth") && !schemes.contains("userAuth")
            };
            if !ok {
                wrong.push(format!("{method} {path} -> {schemes:?}"));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "node routes with the wrong security scheme: {wrong:#?}"
    );
}

/// A field of a Rust DTO, as far as the OpenAPI schema cares: its JSON name,
/// whether it is an `Option` (so `null` is meaningful), and whether it carries
/// `#[serde(default)]` (so omitting it is allowed even when it is not an
/// `Option`).
struct Field {
    name: String,
    optional: bool,
    has_default: bool,
}

/// Fields of `pub struct <name> { .. }` in `crates/common/src/lib.rs`.
fn rust_struct_fields(src: &str, struct_name: &str) -> Vec<Field> {
    let marker = format!("pub struct {struct_name} {{");
    let start = src
        .find(&marker)
        .unwrap_or_else(|| panic!("no `{marker}` in crates/common/src/lib.rs"));
    let body = &src[start + marker.len()..];
    let end = body
        .find("\n}")
        .expect("struct must be closed by a `}` line");
    let body = &body[..end];

    let mut fields = Vec::new();
    let mut pending_default = false;
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with("#[serde(") {
            pending_default |= t.contains("default");
            continue;
        }
        if t.starts_with('#') || t.starts_with("//") || t.is_empty() {
            continue;
        }
        let Some(rest) = t.strip_prefix("pub ") else {
            continue;
        };
        let Some((name, ty)) = rest.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        fields.push(Field {
            name: name.to_string(),
            optional: ty.trim_start().starts_with("Option<"),
            has_default: pending_default,
        });
        pending_default = false;
    }
    assert!(!fields.is_empty(), "parsed zero fields for {struct_name}");
    fields
}

fn documented_properties(doc: &Value, schema: &str) -> BTreeSet<String> {
    doc["components"]["schemas"][schema]["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("schema `{schema}` is missing or has no `properties:` map"))
        .keys()
        .cloned()
        .collect()
}

fn documented_required(doc: &Value, schema: &str) -> BTreeSet<String> {
    doc["components"]["schemas"][schema]["required"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Field-by-field parity for one DTO. Checks both directions, so a new Rust
/// field cannot appear undocumented and a stale doc field cannot survive a
/// rename.
fn assert_dto_matches_rust(doc: &Value, src: &str, struct_name: &str) {
    let fields = rust_struct_fields(src, struct_name);
    let rust_names: BTreeSet<String> = fields.iter().map(|f| f.name.clone()).collect();
    let doc_names = documented_properties(doc, struct_name);

    let undocumented: Vec<&String> = rust_names.difference(&doc_names).collect();
    let stale: Vec<&String> = doc_names.difference(&rust_names).collect();
    assert!(
        undocumented.is_empty() && stale.is_empty(),
        "components.schemas.{struct_name} is out of sync with the Rust struct\n  \
         in Rust but not documented: {undocumented:?}\n  \
         documented but not in Rust: {stale:?}"
    );

    let rust_required: BTreeSet<String> = fields
        .iter()
        .filter(|f| !f.optional && !f.has_default)
        .map(|f| f.name.clone())
        .collect();
    let doc_required = documented_required(doc, struct_name);
    assert_eq!(
        rust_required, doc_required,
        "components.schemas.{struct_name}.required does not match the Rust struct \
         (required = non-Option fields without #[serde(default)])"
    );
}

#[test]
fn create_task_request_schema_matches_rust() {
    let doc = openapi_doc();
    let src = repo_file("crates/common/src/lib.rs");
    assert_dto_matches_rust(&doc, &src, "CreateTaskRequest");
}

#[test]
fn task_view_schema_matches_rust() {
    let doc = openapi_doc();
    let src = repo_file("crates/common/src/lib.rs");
    assert_dto_matches_rust(&doc, &src, "TaskView");
}

// ---------------------------------------------------------------------------
// SDK parity
//
// The two hand-written SDKs (`sdks/python/agentgrid.py`, `sdks/ts/index.ts`)
// are clients of this same contract, but nothing about them is generated:
// `run()` exposed 7 of the 26 `CreateTaskRequest` fields, so `max_attempts`,
// both budget ceilings and `retry_on_node_loss` were unreachable from either
// SDK — a silent feature gap, not a compile error. These tests read the
// declared field lists out of the SDK sources and compare them with the
// OpenAPI schemas (themselves pinned to the Rust DTOs above), so adding a field
// to the API without plumbing it through both SDKs fails here instead of in a
// user's script.
// ---------------------------------------------------------------------------

/// `prompt` and `repository` are positional parameters of `run()` in both
/// SDKs, so they are not part of the parsed field lists.
fn positional_run_args() -> BTreeSet<String> {
    ["prompt", "repository"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Keyword parameters of `def run(` in the Python SDK. The body of the method
/// is a single `_compact(body)` call, so the signature *is* the wire surface.
fn python_run_kwargs(src: &str) -> BTreeSet<String> {
    let marker = "\n    def run(\n";
    let start = src
        .find(marker)
        .expect("sdks/python/agentgrid.py must define `    def run(`");
    let body = &src[start..];
    let end = body
        .find("\n    ) -> ")
        .expect("`def run(` must close with a dedented `    ) -> ...` line");
    let mut out = BTreeSet::new();
    for line in body[..end].lines() {
        let t = line.trim().trim_end_matches(',');
        let Some((name, _)) = t.split_once(':') else {
            continue;
        };
        let name = name.trim().trim_start_matches('*');
        if is_ident(name) && name != "self" {
            out.insert(name.to_string());
        }
    }
    assert!(
        !out.is_empty(),
        "parsed zero keyword args from `def run(` in the Python SDK"
    );
    out
}

/// Fields of `class <name>(TypedDict...)` in the Python SDK.
fn python_typed_dict_fields(src: &str, name: &str) -> BTreeSet<String> {
    let marker = format!("class {name}(TypedDict");
    let start = src
        .find(&marker)
        .unwrap_or_else(|| panic!("no `{marker}` in sdks/python/agentgrid.py"));
    let mut out = BTreeSet::new();
    for line in src[start..].lines().skip(1) {
        if !line.trim().is_empty() && !line.starts_with(' ') {
            break; // dedented: the class body ended
        }
        let t = line.trim().trim_end_matches(',');
        let Some((field, _)) = t.split_once(':') else {
            continue;
        };
        let field = field.trim();
        // The docstring is prose; only identifier-shaped keys survive this.
        if is_ident(field) {
            out.insert(field.to_string());
        }
    }
    assert!(
        !out.is_empty(),
        "parsed zero fields from `class {name}` in the Python SDK"
    );
    out
}

/// Fields of `export interface <name> { ... }` in the TS SDK.
fn ts_interface_fields(src: &str, name: &str) -> BTreeSet<String> {
    let marker = format!("export interface {name} {{");
    let start = src
        .find(&marker)
        .unwrap_or_else(|| panic!("no `{marker}` in sdks/ts/index.ts"));
    let body = &src[start + marker.len()..];
    let end = body
        .find("\n}")
        .unwrap_or_else(|| panic!("`interface {name}` must be closed by a `}}` line"));
    let mut out = BTreeSet::new();
    for line in body[..end].lines() {
        let t = line.trim();
        if t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
            continue;
        }
        // `field?: type;` / `field: type;` — a bare comment or a nested block
        // does not end with `;` and is skipped.
        let Some(rest) = t.strip_suffix(';') else {
            continue;
        };
        let Some((field, _)) = rest.split_once(':') else {
            continue;
        };
        let field = field.trim().trim_end_matches('?');
        if is_ident(field) {
            out.insert(field.to_string());
        }
    }
    assert!(
        !out.is_empty(),
        "parsed zero fields from `interface {name}` in the TS SDK"
    );
    out
}

fn assert_sdk_covers(schema: &str, sdk: &str, fields: &BTreeSet<String>) {
    let api = documented_properties(&openapi_doc(), schema);
    let missing: Vec<&String> = api.difference(fields).collect();
    let extra: Vec<&String> = fields.difference(&api).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{sdk} is out of sync with components.schemas.{schema}\n  \
         in the API but not in the SDK: {missing:?}\n  \
         in the SDK but not in the API: {extra:?}"
    );
}

#[test]
fn python_sdk_run_covers_create_task_request() {
    let src = repo_file("sdks/python/agentgrid.py");
    let mut fields = python_run_kwargs(&src);
    fields.extend(positional_run_args());
    assert_sdk_covers("CreateTaskRequest", "Python `Agentgrid.run()`", &fields);
}

#[test]
fn ts_sdk_run_covers_create_task_request() {
    let src = repo_file("sdks/ts/index.ts");
    let mut fields = ts_interface_fields(&src, "CreateTaskInput");
    fields.extend(positional_run_args());
    assert_sdk_covers("CreateTaskRequest", "TS `CreateTaskInput`", &fields);
}

#[test]
fn python_sdk_task_covers_task_view() {
    let src = repo_file("sdks/python/agentgrid.py");
    let fields = python_typed_dict_fields(&src, "Task");
    assert_sdk_covers("TaskView", "Python `Task`", &fields);
}

#[test]
fn ts_sdk_task_covers_task_view() {
    let src = repo_file("sdks/ts/index.ts");
    let fields = ts_interface_fields(&src, "Task");
    assert_sdk_covers("TaskView", "TS `Task`", &fields);
}

/// The SDKs ship with the server they were written against, so their package
/// versions track the crate version. They sat at `0.1.0` while the workspace
/// moved to 0.5.x — the same rot `info.version` had.
#[test]
fn sdk_package_versions_match_the_workspace_version() {
    let want = workspace_version();
    let pyproject = repo_file("sdks/python/pyproject.toml");
    let mut in_project = false;
    let mut python_version = None;
    for line in pyproject.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_project = t == "[project]";
            continue;
        }
        if in_project {
            if let Some(v) = t.strip_prefix("version") {
                python_version = Some(
                    v.trim_start()
                        .trim_start_matches('=')
                        .trim()
                        .trim_matches('"')
                        .to_string(),
                );
                break;
            }
        }
    }
    let python_version = python_version.expect("sdks/python/pyproject.toml [project] version");

    let package_json = repo_file("sdks/ts/package.json");
    let idx = package_json
        .find("\"version\"")
        .expect("sdks/ts/package.json must declare a version");
    let rest = &package_json[idx + "\"version\"".len()..];
    let start = rest.find('"').expect("package.json version value") + 1;
    let end = rest[start..].find('"').expect("package.json version value") + start;
    let ts_version = &rest[start..end];

    assert_eq!(
        (python_version.as_str(), ts_version),
        (want.as_str(), want.as_str()),
        "SDK package versions must equal the workspace crate version ({want}); \
         bump sdks/python/pyproject.toml and sdks/ts/package.json with it"
    );
}
