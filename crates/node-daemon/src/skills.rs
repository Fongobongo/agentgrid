//! Skills and context composition: discover operator-trusted skills in the
//! worktree + user home and build the attempt context pack.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use agentgrid_common::{Assignment, ContextProvider, EventType};
use agentgrid_skills::DiscoveredSkill;
use reqwest::Client;
use serde_json::json;

use crate::event_sink::EventSink;

/// Discover skills in the worktree + user home, keep only the ones
/// the operator explicitly trusted on the control plane (fail-closed: an
/// untrusted/unknown skill is omitted), and render a short "Available skills"
/// block to append to the prompt. Returns an empty string on any error so the
/// task is never blocked by the trust lookup wiring (the skills are a hint, not
/// a hard dependency).
///
/// `ponytail:` fetches the whole trust ledger per attempt (O(skills) over HTTP,
/// small); if skill counts grow, switch to a per-skill lookup or a node-side
/// cache keyed by `(name, source)`.
pub async fn compose_skills_block(client: &Client, server: &str, ws_path: &Path) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let roots = agentgrid_skills::standard_roots(ws_path, home.as_deref());
    let (discovered, _diags) = agentgrid_skills::discover(&roots);
    if discovered.is_empty() {
        return String::new();
    }
    // Fetch the trust ledger; untrusted/absent entries are dropped.
    // Uses the node-authenticated `/v1/node/skills-trust` mirror with the
    // process-stashed node credential (the operator `/v1/skills` is user-JWT
    // only; a bare GET 401s and the block would silently stay empty).
    let trusted_name: HashSet<(String, String)> = {
        let req = client.get(format!("{server}/v1/node/skills-trust"));
        let req = match crate::config::process_credential() {
            Some(c) => req.bearer_auth(&c.credential),
            None => req,
        };
        match req.send().await {
            Ok(r) if r.status().is_success() => {
                match r.json::<Vec<agentgrid_common::SkillTrustView>>().await {
                    Ok(rows) => rows
                        .into_iter()
                        .filter(|v| v.trusted)
                        .map(|v| (v.name, v.source))
                        .collect(),
                    Err(_) => return String::new(),
                }
            }
            _ => return String::new(), // skills are a hint; don't block the task
        }
    };
    render_trusted_skills_block(&discovered, &trusted_name)
}

/// Stage 11 (CTX): build a context pack for the attempt's repo+base_commit
/// via the configured `ContextProvider` (default Noop → empty pack), append its
/// body to the prompt, and stream a `context_pack` status event with the
/// before/after bytes + cache-hit metrics. Any provider error is swallowed:
/// the agent simply proceeds without a context digest (graceful fallback).
///
/// `ponytail:` single provider instance, no on-disk cache yet; the cache key
/// is computed by the provider so a future CTX impl can consult a warm cache
/// on disk and skip re-indexing (Stage 11 exit criterion).
pub async fn compose_context_block(
    provider: &dyn ContextProvider,
    assignment: &Assignment,
    sink: &Arc<EventSink>,
) -> String {
    let repo = assignment.repository.as_str();
    let base = assignment.base_commit.as_deref().unwrap_or("");
    let pack = match provider.build(repo, base) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                provider = provider.id(),
                "context provider failed: {e}; falling back to no pack"
            );
            return String::new();
        }
    };
    if pack.is_empty() {
        return String::new();
    }
    // Metrics event: bytes_in/bytes_out/cache_hit/index_ms.
    sink.push(
        EventType::Status,
        json!({
            "kind": "context_pack",
            "provider": pack.provider,
            "repo": pack.repo,
            "base_commit": pack.base_commit,
            "cache_key": pack.cache_key,
            "cache_hit": pack.cache_hit,
            "bytes_in": pack.bytes_in,
            "bytes_out": pack.bytes_out,
            "index_ms": pack.index_ms,
        }),
    )
    .await;
    pack.body
}

/// Pure render of the trusted subset of discovered skills. Separated so it can
/// be unit-tested without HTTP.
pub fn render_trusted_skills_block(
    discovered: &[DiscoveredSkill],
    trusted: &HashSet<(String, String)>,
) -> String {
    let mut keep: Vec<&DiscoveredSkill> = discovered
        .iter()
        .filter(|d| trusted.contains(&(d.skill.name.clone(), d.source.as_str().to_string())))
        .collect();
    if keep.is_empty() {
        return String::new();
    }
    keep.sort_by(|a, b| a.skill.name.cmp(&b.skill.name));
    let mut out = String::from("\n\nAvailable agent skills (operator-trusted):\n");
    for d in keep {
        out.push_str(&format!(
            "- {} ({}): {}\n",
            d.skill.name,
            d.source.as_str(),
            d.skill.description.lines().next().unwrap_or("").trim(),
        ));
    }
    out
}
