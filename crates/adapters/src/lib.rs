//! Adapter contract.
//!
//! An adapter is a subprocess launched by the node daemon. It emits
//! newline-delimited JSON events to stdout. Unrecognized stdout lines are
//! treated as raw logs by the daemon (never a fatal error).
//!
//! Contract event `type` values: `log | tool_call | file_change | progress |
//! result | error`.

use agentgrid_common::EventType;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterEvent {
    pub r#type: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Map an adapter contract `type` string to a stored [`EventType`].
/// Unknown types fall back to `Stdout` (raw log) per spec 3.1.
pub fn to_event_type(t: &str) -> EventType {
    match t {
        "log" => EventType::Stdout,
        "tool_call" => EventType::Tool,
        "file_change" => EventType::Artifact,
        "progress" => EventType::Metric,
        "result" => EventType::Result,
        "error" => EventType::Error,
        _ => EventType::Stdout,
    }
}
