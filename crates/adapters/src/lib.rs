//! Adapter contract (finalized, Stage 3.1).
//!
//! An adapter is a **subprocess** launched by the node daemon. It runs the
//! actual coding agent and reports progress by writing newline-delimited JSON
//! (NDJSON) events to **stdout**. The daemon parses each line into a streamed
//! [`agentgrid_common::TaskEvent`]; unrecognized stdout lines are treated as
//! raw logs (never a fatal error), so a future CLI output-format change cannot
//! break the pipeline — the raw output is always preserved as an artifact.
//!
//! Lifecycle the daemon drives (conceptual `prepare/start/stream/cancel/collect`):
//! - **prepare**: the daemon creates a per-attempt git worktree and sets `cwd`.
//! - **start**: the daemon spawns the adapter binary with `--prompt <prompt>`
//!   and any forwarded env (e.g. API keys from `AGENTGRID_ADAPTER_ENV`).
//! - **stream**: the adapter writes NDJSON events to stdout until it exits.
//! - **cancel**: the daemon SIGTERMs the adapter's process group (SIGKILL after
//!   a 10s grace); the adapter need not handle signals specially.
//! - **collect**: on exit the daemon captures the commit SHA, runs the optional
//!   validation command, and uploads artifacts (`changes.patch`,
//!   `validation.log`, `agent-raw-output.log`).
//!
//! Contract event `type` values: `log | tool_call | file_change | progress |
//! result | error`. Unknown types fall back to `Stdout` (raw log) per spec 3.1.

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
