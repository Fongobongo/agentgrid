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
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
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
