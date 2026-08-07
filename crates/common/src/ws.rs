//! Plan 0.3 stage 2: node WebSocket control channel (`/v1/node/ws`).
//!
//! Shared envelope types for the CP ↔ node WS channel (ADR 0009, message
//! table in `docs/node-ws-protocol.md`). The channel carries only control
//! messages — assignment push, ack, cancel, heartbeat; the data plane
//! (events, completion, artifacts) stays on the HTTP endpoints.

use serde::{Deserialize, Serialize};

use crate::Assignment;

/// JSON envelope on the node WS channel. Serialized as
/// `{"type": "<snake_case variant>", ...}`. One enum covers both directions;
/// each side only sends its own variants (the CP never sends `hello`, a node
/// never sends `assignment`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeWsMsg {
    /// Node → CP: session registration, sent first after the upgrade.
    /// Capability fields mirror `PollRequest` (same semantics).
    Hello {
        node_id: String,
        name: String,
        adapters: Vec<String>,
        repositories: Vec<String>,
        max_concurrency: u32,
        #[serde(default)]
        protocol_version: Option<String>,
        #[serde(default)]
        agent_version: String,
    },
    /// CP → node: session accepted; the node is in the push registry.
    HelloOk { server_time: i64 },
    /// CP → node: batch of fresh assignments (same payload as
    /// `PollResponse.assignments`, fencing tokens included).
    Assignment { assignments: Vec<Assignment> },
    /// Node → CP: received assignment(s). `ok=false` rejects the attempts
    /// (they are completed failed so the tasks requeue via retry).
    /// `fencing_tokens` parallels `attempt_ids` (plan 0.3 2.4: fencing on the
    /// WS path); legacy nodes omit it and are checked as token-less.
    Ack {
        attempt_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fencing_tokens: Vec<String>,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// CP → node: cancel requested for an attempt (`cancel_requested` is
    /// already set in the store; the push only speeds up delivery).
    Cancel { attempt_id: String },
    /// Node → CP: cancel seen and acted upon.
    CancelAck { attempt_id: String },
    /// Node → CP: free slots changed; the CP may push more work.
    Heartbeat { free_slots: u32 },
}

/// WS close codes for the node channel (docs/node-ws-protocol.md).
pub const WS_CLOSE_UNAUTHORIZED: u16 = 4001;
pub const WS_CLOSE_BAD_PROTOCOL: u16 = 4002;
pub const WS_CLOSE_SUPERSEDED: u16 = 4003;
pub const WS_CLOSE_DRAIN: u16 = 4004;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tag_names() {
        let m = NodeWsMsg::Ack {
            attempt_ids: vec!["a1".into()],
            fencing_tokens: vec!["t1".into()],
            ok: true,
            error: None,
        };
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.starts_with("{\"type\":\"ack\""), "got: {j}");
        let back: NodeWsMsg = serde_json::from_str(&j).unwrap();
        assert_eq!(back, m);

        let a = NodeWsMsg::Assignment {
            assignments: vec![Assignment::default()],
        };
        let j = serde_json::to_string(&a).unwrap();
        assert!(j.contains("\"type\":\"assignment\""));
        let back: NodeWsMsg = serde_json::from_str(&j).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn legacy_hello_without_optional_fields_parses() {
        let j = r#"{"type":"hello","node_id":"n","name":"n","adapters":[],"repositories":[],"max_concurrency":1}"#;
        let m: NodeWsMsg = serde_json::from_str(j).unwrap();
        match m {
            NodeWsMsg::Hello {
                protocol_version,
                agent_version,
                ..
            } => {
                assert_eq!(protocol_version, None);
                assert_eq!(agent_version, "");
            }
            _ => panic!("expected hello"),
        }
    }
}
