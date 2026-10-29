//! ACP method parameters/results and the `session/update` → `AgentEventEnvelope`
//! mapping. Kept intentionally small: the southbound client only needs to
//! construct requests and normalize inbound updates into the common envelope.

use agentgrid_common::{AgentEventEnvelope, EventKind};
use serde::Serialize;
use serde_json::{json, Value};

use crate::codec::Id;

pub const METHOD_INITIALIZE: &str = "initialize";
pub const METHOD_SESSION_NEW: &str = "session/new";
pub const METHOD_SESSION_PROMPT: &str = "session/prompt";
pub const METHOD_SESSION_CANCEL: &str = "session/cancel";
pub const METHOD_SESSION_UPDATE: &str = "session/update";
pub const METHOD_SESSION_REQUEST_PERMISSION: &str = "session/request_permission";

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct InitializeParams {
    pub protocol_version: String,
    pub agent: String,
    pub model: String,
    #[serde(default)]
    pub session_id: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub capabilities: Value,
    #[serde(default)]
    pub client: Value,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct InitializeResult {
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: Value,
    #[serde(default)]
    pub server_info: Value,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SessionNewParams {
    pub agent: String,
    #[serde(default)]
    pub model: Option<String>,
    /// Absolute working directory (plan: absolute paths passed through).
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// MCP stdio servers to forward into the session config.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub mcp: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SessionNewResult {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SessionPromptParams {
    pub session_id: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SessionCancelParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SessionRequestPermissionParams {
    pub session_id: String,
    /// Tool / action the agent is asking to run (fail-closed if unapproved).
    pub permission: Value,
}

/// Map an ACP `update.type` onto the normalized [`EventKind`]. Unknown types
/// are preserved verbatim (never fatal), matching the 3.1 envelope contract.
pub fn update_type_to_kind(t: &str) -> EventKind {
    match t {
        "plan" => EventKind::Plan,
        "tool_call" => EventKind::ToolCall,
        "tool_result" => EventKind::ToolResult,
        "diff" => EventKind::FileChange,
        "file_change" => EventKind::FileChange,
        "usage" => EventKind::Usage,
        // ACP `usage_update` is the spec'd spelling; `usage` is what the
        // wrapper adapters emit. Both carry spend/context telemetry.
        "usage_update" => EventKind::Usage,
        "log" => EventKind::Log,
        "permission_request" => EventKind::PermissionRequest,
        "progress" => EventKind::Progress,
        "result" => EventKind::Result,
        "error" => EventKind::Error,
        "status" => EventKind::Status,
        "handoff" => EventKind::Handoff,
        "cancel" => EventKind::Cancel,
        other => EventKind::Other(other.to_string()),
    }
}

/// Normalize a `session/update` notification into the common envelope.
/// `payload` is the full `update` object (carries the agent's own fields).
pub fn map_session_update(session_id: &str, update: &Value) -> AgentEventEnvelope {
    let kind = update
        .get("type")
        .and_then(|t| t.as_str())
        .map(update_type_to_kind)
        .unwrap_or(EventKind::Other("unknown".into()));
    let mut payload = update.clone();
    // Stamp the session so the control plane can correlate without re-parsing.
    if let Value::Object(ref mut map) = payload {
        map.entry("session_id".to_string())
            .or_insert(json!(session_id));
    }
    if kind == EventKind::Usage {
        normalize_usage_payload(&mut payload);
    }
    AgentEventEnvelope {
        version: 1,
        kind,
        payload,
        raw_ref: None,
    }
}

/// Normalize an ACP `usage_update` into the budget path's vocabulary.
///
/// The spec shape is `{ used, size, cost? }` where `cost` is
/// `{ amount, currency }`. Both numbers are **running session totals**, not
/// per-event increments, but they are different *kinds* of total and so are
/// normalized differently:
///
/// * `used` is context-window occupancy — a gauge that rises as the
///   conversation grows and **falls** when the agent compacts or truncates.
///   It is published as `tokens` flagged `tokens_gauge: true`; the node turns
///   successive readings into positive increments, the closest thing to "tokens
///   spent" this protocol exposes. The raw reading stays in `used`.
/// * `cost.amount` is already a monotone cumulative total. It becomes integer
///   `cost_cents` flagged `cost_cumulative: true`, and the node deltas it;
///   summing the raw value on every update would inflate reported spend
///   quadratically (0.05, 0.12, 0.20 → 0.37 instead of 0.20).
///
/// Only USD is converted — there is no FX table here, and silently treating
/// EUR as USD would be a wrong-by-a-factor cost ceiling. The original `cost`
/// object stays in the payload so an operator can see what was skipped.
fn normalize_usage_payload(payload: &mut Value) {
    let Value::Object(map) = payload else { return };
    normalize_usage_tokens(map);
    normalize_usage_cost(map);
}

/// Publish the context gauge as `tokens` plus a `tokens_gauge` marker.
///
/// A payload that already carries `tokens` (wrapper adapters, agent
/// extensions) is left alone: an explicit per-event count is a delta and must
/// not be re-interpreted as a gauge reading.
fn normalize_usage_tokens(map: &mut serde_json::Map<String, Value>) {
    if map.contains_key("tokens") {
        return;
    }
    let Some(used) = map.get("used").and_then(json_u64) else {
        return;
    };
    map.insert("tokens".to_string(), json!(used));
    map.insert("tokens_gauge".to_string(), json!(true));
}

fn normalize_usage_cost(map: &mut serde_json::Map<String, Value>) {
    // An explicit per-event `cost_cents` (wrapper adapters, agent extensions)
    // is already a delta — leave it alone.
    if map.contains_key("cost_cents") {
        return;
    }
    let Some(cost) = map.get("cost").and_then(|c| c.as_object()) else {
        return;
    };
    let amount = cost.get("amount").and_then(|a| a.as_f64()).unwrap_or(0.0);
    let currency = cost
        .get("currency")
        .and_then(|c| c.as_str())
        .unwrap_or("USD");
    if amount <= 0.0 || !currency.eq_ignore_ascii_case("USD") {
        return;
    }
    map.insert(
        "cost_cents".to_string(),
        json!((amount * 100.0).round() as u64),
    );
    map.insert("cost_cumulative".to_string(), json!(true));
}

/// Read a JSON number as a non-negative integer, tolerating the float form
/// some agents use for token counts.
fn json_u64(v: &Value) -> Option<u64> {
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    let f = v.as_f64()?;
    if !f.is_finite() || f < 0.0 {
        return None;
    }
    Some(f.round() as u64)
}

/// Build a `session/update` notification message (used by tests / fakes).
pub fn session_update_message(session_id: &str, update: Value) -> crate::codec::Message {
    crate::codec::Message::Notification {
        method: METHOD_SESSION_UPDATE.into(),
        params: json!({ "session_id": session_id, "update": update }),
    }
}

/// Convenience: build a `session/new` request.
pub fn session_new_request(id: Id, params: SessionNewParams) -> crate::codec::Message {
    crate::codec::Message::Request {
        id,
        method: METHOD_SESSION_NEW.into(),
        params: serde_json::to_value(params).expect("params serialize"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_and_unknown_update_types() {
        let u = json!({ "type": "tool_call", "name": "Bash" });
        let env = map_session_update("s1", &u);
        assert_eq!(env.kind, EventKind::ToolCall);
        assert_eq!(env.payload.get("session_id").unwrap(), &json!("s1"));

        let u2 = json!({ "type": "weird_thing" });
        assert_eq!(
            map_session_update("s1", &u2).kind,
            EventKind::Other("weird_thing".into())
        );

        let u3 = json!({ "type": "diff" });
        assert_eq!(map_session_update("s1", &u3).kind, EventKind::FileChange);
    }

    #[test]
    fn usage_update_normalizes_cumulative_usd_cost_to_cents() {
        // Spec shape: context gauge + cumulative cost. The cumulative amount
        // must be flagged so the node deltas it instead of summing it.
        let u = json!({
            "type": "usage_update",
            "used": 1200,
            "size": 200_000,
            "cost": { "amount": 0.0523, "currency": "USD" }
        });
        let env = map_session_update("s1", &u);
        assert_eq!(env.kind, EventKind::Usage);
        assert_eq!(env.payload.get("cost_cents"), Some(&json!(5)));
        assert_eq!(env.payload.get("cost_cumulative"), Some(&json!(true)));
        // The raw gauge survives for observability.
        assert_eq!(env.payload.get("used"), Some(&json!(1200)));
        assert_eq!(env.payload.get("size"), Some(&json!(200_000)));
    }

    #[test]
    fn usage_update_publishes_the_context_gauge_as_flagged_tokens() {
        // `used` is occupancy, not spend: it is handed to the node as a gauge
        // (flagged) so the node can turn successive readings into increments.
        // A naive `tokens: used` would feed a `max_tokens` ceiling a value that
        // grows with the context window rather than with real consumption.
        let u = json!({
            "type": "usage_update",
            "used": 1200,
            "size": 200_000,
            "cost": { "amount": 0.05, "currency": "USD" }
        });
        let env = map_session_update("s1", &u);
        assert_eq!(env.payload.get("tokens"), Some(&json!(1200)));
        assert_eq!(env.payload.get("tokens_gauge"), Some(&json!(true)));
        assert_eq!(env.payload.get("used"), Some(&json!(1200)));
    }

    #[test]
    fn usage_update_without_cost_still_publishes_tokens() {
        // Cost and tokens are independent: an agent that reports occupancy but
        // no price must still feed the token ceiling. This used to fall out
        // early because the cost branch was the only entry point.
        let u = json!({ "type": "usage_update", "used": 900, "size": 100_000 });
        let env = map_session_update("s1", &u);
        assert_eq!(env.kind, EventKind::Usage);
        assert_eq!(env.payload.get("tokens"), Some(&json!(900)));
        assert_eq!(env.payload.get("tokens_gauge"), Some(&json!(true)));
        assert!(env.payload.get("cost_cents").is_none());
    }

    #[test]
    fn explicit_tokens_are_left_as_deltas() {
        // A wrapper adapter's per-event count must not be re-read as a gauge.
        let u = json!({ "type": "usage", "tokens": 42, "used": 9999 });
        let env = map_session_update("s1", &u);
        assert_eq!(env.payload.get("tokens"), Some(&json!(42)));
        assert!(env.payload.get("tokens_gauge").is_none());
    }

    #[test]
    fn usage_update_accepts_a_float_gauge() {
        let u = json!({ "type": "usage_update", "used": 1200.6 });
        let env = map_session_update("s1", &u);
        assert_eq!(env.payload.get("tokens"), Some(&json!(1201)));
        assert_eq!(env.payload.get("tokens_gauge"), Some(&json!(true)));
    }

    #[test]
    fn usage_update_leaves_non_usd_and_explicit_costs_untouched() {
        // No FX table: EUR must not be passed off as USD cents.
        let eur = json!({
            "type": "usage_update",
            "used": 10,
            "size": 100,
            "cost": { "amount": 1.5, "currency": "EUR" }
        });
        let env = map_session_update("s1", &eur);
        assert_eq!(env.kind, EventKind::Usage);
        assert!(env.payload.get("cost_cents").is_none());
        assert!(env.payload.get("cost").is_some(), "original cost kept");

        // A per-event `cost_cents` from an extension is already a delta.
        let explicit = json!({
            "type": "usage",
            "cost_cents": 7,
            "cost": { "amount": 9.0, "currency": "USD" }
        });
        let env2 = map_session_update("s1", &explicit);
        assert_eq!(env2.payload.get("cost_cents"), Some(&json!(7)));
        assert!(env2.payload.get("cost_cumulative").is_none());
    }

    #[test]
    fn session_new_serializes_paths_and_mcp() {
        let p = SessionNewParams {
            agent: "opencode".into(),
            model: Some("gpt".into()),
            cwd: "/abs/path".into(),
            prompt: None,
            mcp: json!({ "servers": ["x"] }),
            parent_session_id: None,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v.get("cwd").unwrap(), &json!("/abs/path"));
        assert!(v.get("mcp").is_some());
    }
}
