//! Shared lifecycle-phase logic for `ag logs` and the TUI (single source of
//! truth; previously copy-pasted between `main.rs` and `tui.rs`).

/// Render lifecycle phase derived from the event stream + pending approvals,
/// orthogonal to the terminal `TaskStatus`. Mirrors the herdr agent-state idea
/// (`idle | working | blocked | done`) but computed client-side from events
/// the control plane already emits, so no store/migration change is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    /// No structured events yet seen (just stdout/stderr).
    #[default]
    Starting,
    /// Last structured event was a tool call / progress / file change.
    Working,
    /// A durable approval is pending for this task (or the stream says so).
    Blocked,
    /// Vertically terminal — set by callers once `TaskStatus` is terminal.
    Done,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::Working => "working",
            Phase::Blocked => "blocked",
            Phase::Done => "done",
        }
    }

    /// Map one NDJSON task event to the lifecycle phase it implies.
    pub fn from_event(ty: &str, e: &serde_json::Value) -> Self {
        match ty {
            "tool" | "tool_call" | "file_change" | "progress" | "stdout" | "stderr" => {
                Phase::Working
            }
            "result" | "error" => Phase::Done,
            "status" => {
                // a status event with a terminal-ish payload hints at done; default Working.
                if let Some(t) = e
                    .get("payload")
                    .and_then(|p| p.get("text"))
                    .and_then(|t| t.as_str())
                {
                    if t.contains("succeeded") || t.contains("failed") || t.contains("cancelled") {
                        return Phase::Done;
                    }
                }
                Phase::Working
            }
            _ => Phase::Starting,
        }
    }
}
