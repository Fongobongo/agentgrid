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

mod backend;
pub use backend::{
    classify_exit, BackendOutcome, BackendProcess, ExecutionBackend, ProcessBackend,
    ResourceLimits, SpawnRequest,
};

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

/// Extract fenced ` ```plan ` code blocks from an agent's text output
/// (Stage 13 plan approval). A workflow architect instructed to wrap its
/// machine-readable plan in such a fence gets it surfaced as a `plan` event;
/// the last block wins downstream. Unclosed fences are ignored.
pub fn plan_blocks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_plan = false;
    let mut cur = String::new();
    for line in text.lines() {
        let t = line.trim();
        if !in_plan && (t == "```plan" || t.starts_with("```plan ")) {
            in_plan = true;
            cur.clear();
        } else if in_plan && t == "```" {
            in_plan = false;
            out.push(cur.trim().to_string());
        } else if in_plan {
            if !cur.is_empty() {
                cur.push('\n');
            }
            cur.push_str(line);
        }
    }
    out.retain(|p| !p.is_empty());
    out
}

/// Hardening P0 (unsafe adapter defaults): resolve whether an adapter may
/// run its "unsafe unattended" mode — bypassing interactive permission
/// prompts / auto-running every tool call — under a single operator opt-in
/// `AGENTGRID_UNSAFE_UNATTENDED=1` (or `true`, case-insensitive). Returns
/// `(unsafe_unattended, warned_label)`. Default off = safe: callers must NOT
/// add their dangerous flag/arg unless this returns true.
///
/// `AGENTGRID_UNSAFE_UNATTENDED` is the only knob that should turn unsafe mode
/// on by default; per-adapter knobs may *also* enable it but are loudly warned.
pub fn unsafe_unattended_from_env() -> bool {
    std::env::var("AGENTGRID_UNSAFE_UNATTENDED")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Operator acknowledgement that unsafe unattended mode runs without a
/// sandbox and with permissions bypassed. Unsafe mode requested via
/// `AGENTGRID_UNSAFE_UNATTENDED` must be paired with this flag or the
/// node daemon refuses to start (fail-closed).
pub fn unsafe_ack_from_env() -> bool {
    std::env::var("AGENTGRID_I_UNDERSTAND_UNSAFE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Build the base argv launched against the Claude Code CLI, gated on
/// [`unsafe_unattended_from_env`]. `--dangerously-skip-permissions` is added
/// only when unsafe unattended mode is on; by default the adapter never adds
/// the dangerous skip flag (hardening P0). The adapter prints the matching
/// warning itself via [`warn_unsafe`].
pub fn claude_args(prompt: &str, unsafe_unattended: bool) -> Vec<String> {
    let mut args = vec![
        "-p".into(),
        prompt.to_string(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
    ];
    if unsafe_unattended {
        args.push("--dangerously-skip-permissions".into());
    }
    args
}

/// Build the base argv launched against the opencode CLI. `--auto` (auto-run
/// every tool call) is added only under unsafe-unattended, or when the legacy
/// `AGENTGRID_OPENCODE_AUTO` knob opts in (default off). Hardening P0 + P1.
pub fn opencode_auto(unsafe_unattended: bool) -> bool {
    if unsafe_unattended {
        return true;
    }
    std::env::var("AGENTGRID_OPENCODE_AUTO")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Emit the stderr warning line for the chosen safety mode. `adapter` is a
/// short label ("adapter-claude" / "adapter-opencode"); `flag` names the
/// dangerous opt being applied (or missing).
pub fn warn_unsafe(adapter: &str, unsafe_unattended: bool, auto_via_knob: bool) {
    if unsafe_unattended {
        eprintln!(
            "{adapter}: WARNING — AGENTGRID_UNSAFE_UNATTENDED=1 bypasses interactive permissions/auto-run. Do NOT set this without a sandbox."
        );
    } else if auto_via_knob {
        eprintln!(
            "{adapter}: WARNING — per-adapter auto knobs are on without AGENTGRID_UNSAFE_UNATTENDED. Prefer the single unsafe knob with a sandbox."
        );
    } else {
        eprintln!(
            "{adapter}: safe mode (no dangerous bypass); an unattended run will block on the first prompt unless AGENTGRID_UNSAFE_UNATTENDED=1."
        );
    }
}

#[cfg(test)]
mod unsafe_tests {
    use super::{claude_args, opencode_auto, unsafe_unattended_from_env};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env(val: Option<&str>, f: impl FnOnce()) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("AGENTGRID_UNSAFE_UNATTENDED").ok();
        match val {
            Some(v) => std::env::set_var("AGENTGRID_UNSAFE_UNATTENDED", v),
            None => std::env::remove_var("AGENTGRID_UNSAFE_UNATTENDED"),
        }
        f();
        match prev {
            Some(p) => std::env::set_var("AGENTGRID_UNSAFE_UNATTENDED", p),
            None => std::env::remove_var("AGENTGRID_UNSAFE_UNATTENDED"),
        }
    }

    #[test]
    fn defaults_off_when_unset() {
        with_env(None, || assert!(!unsafe_unattended_from_env()));
    }

    #[test]
    fn off_for_zero_and_false_and_garbage() {
        for v in ["0", "false", "FALSE", "no", ""] {
            with_env(Some(v), || {
                assert!(!unsafe_unattended_from_env(), "val={v}")
            });
        }
    }

    #[test]
    fn on_for_one_and_true_case_insensitive() {
        for v in ["1", "true", "TRUE", "True"] {
            with_env(Some(v), || assert!(unsafe_unattended_from_env(), "val={v}"));
        }
    }

    #[test]
    fn claude_default_args_no_skip_permission_flag() {
        // Hardening P0: default command does NOT contain the dangerous flag.
        let args = claude_args("hi", false);
        assert!(!args.iter().any(|a| a == "--dangerously-skip-permissions"));
        assert_eq!(args[0], "-p");
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--output-format" && w[1] == "stream-json"));
    }

    #[test]
    fn claude_unsafe_adds_skip_permission_flag() {
        let args = claude_args("hi", true);
        assert!(args.iter().any(|a| a == "--dangerously-skip-permissions"));
    }

    #[test]
    fn opencode_auto_off_by_default() {
        with_env(None, || {
            assert!(!opencode_auto(false), "default must be off");
        });
    }

    #[test]
    fn opencode_auto_via_unsafe_knob() {
        with_env(None, || assert!(opencode_auto(true)));
    }
}

#[cfg(test)]
mod plan_block_tests {
    use super::plan_blocks;

    #[test]
    fn extracts_single_fenced_plan() {
        let text =
            "Here is my plan:\n```plan\n- id: w\n  prompt: do work\n  role: worker\n```\nDone.";
        let plans = plan_blocks(text);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0], "- id: w\n  prompt: do work\n  role: worker");
    }

    #[test]
    fn unclosed_fence_ignored() {
        assert!(plan_blocks("```plan\n- id: w").is_empty());
    }

    #[test]
    fn no_plan_fence_yields_none() {
        assert!(plan_blocks("just some text\n```yaml\nfoo: bar\n```").is_empty());
    }

    #[test]
    fn last_block_kept_when_multiple() {
        let text = "```plan\nA\n```\nmid\n```plan\nB\n```";
        assert_eq!(plan_blocks(text), vec!["A".to_string(), "B".to_string()]);
    }
}
