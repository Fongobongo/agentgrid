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
    AgentEventEnvelope {
        version: 1,
        kind,
        payload,
        raw_ref: None,
    }
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
