//! Real adapter wrapper for OpenAI Codex CLI (`codex exec`).
//!
//! Launches `codex exec --json` non-interactively and translates its JSONL
//! event stream into the agentgrid event contract (NDJSON on stdout).
//! Unknown lines fall back to a raw `log` event, so a future codex
//! output-format change cannot break the pipeline — the daemon also preserves
//! the raw stdout as the `agent-raw-output.log` artifact.
//!
//! Invocation contract (matches the daemon): `--prompt "<text>"`, run with
//! cwd = attempt worktree. Auth via env (`OPENAI_API_KEY`; custom gateways
//! through `OPENAI_BASE_URL`), forwarded by the daemon from CP-managed
//! adapter env or node-local `AGENTGRID_ADAPTER_ENV`.
//!
//! Safety mapping (same shape as adapter-claude/adapter-opencode):
//!   default     → `codex exec --sandbox workspace-write` (writes allowed,
//!                 no full-host escape, no approval prompts in exec mode)
//!   AGENTGRID_UNSAFE_UNATTENDED=1 →
//!                 `--dangerously-bypass-approvals-and-sandbox` (host trust
//!                 boundary; the operator already opted in globally)

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::json;

fn emit_event(ev: serde_json::Value) {
    let line = serde_json::to_string(&ev).unwrap();
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Build the `codex exec` argv. Separated from main for tests.
fn codex_args(prompt: &str, unsafe_unattended: bool) -> Vec<String> {
    let mut args = vec!["exec".to_string(), "--json".to_string()];
    if unsafe_unattended {
        args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
    } else {
        args.push("--sandbox".to_string());
        args.push("workspace-write".to_string());
    }
    // Worktrees created by the daemon are always git repos; the flag only
    // covers corner cases (archive-based attempts) and is harmless otherwise.
    args.push("--skip-git-repo-check".to_string());
    if let Ok(model) = std::env::var("CODEX_MODEL") {
        if !model.trim().is_empty() {
            args.push("-m".to_string());
            args.push(model);
        }
    }
    args.push(prompt.to_string());
    args
}

/// Translate one `codex exec --json` line into agentgrid events.
///
/// Typical stream (codex 0.150):
///   {"type":"thread.started",...}
///   {"type":"turn.started"}
///   {"type":"item.completed","item":{"type":"reasoning","text":"..."}}
///   {"type":"item.completed","item":{"type":"command_execution",
///        "command":"bash -lc ls","aggregated_output":"...","exit_code":0}}
///   {"type":"item.completed","item":{"type":"agent_message","text":"..."}}
///   {"type":"turn.completed","usage":{"input_tokens":N,...}}
///   {"type":"error","message":"..."}
fn translate(line: &str, saw_error: &mut bool, last_msg: &mut String) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            out.push(json!({ "type": "log", "payload": { "text": line } }));
            return out;
        }
    };
    let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
    match t {
        "item.completed" => {
            let item = v.get("item").cloned().unwrap_or_default();
            match item.get("type").and_then(|x| x.as_str()) {
                Some("agent_message") => {
                    let text = item.get("text").and_then(|x| x.as_str()).unwrap_or("");
                    *last_msg = text.to_string();
                    for plan in agentgrid_adapters::plan_blocks(text) {
                        out.push(json!({ "type": "plan", "payload": { "text": plan } }));
                    }
                    out.push(json!({ "type": "log", "payload": { "text": text } }));
                }
                Some("command_execution") => {
                    let cmd = item.get("command").and_then(|x| x.as_str()).unwrap_or("");
                    let exit = item.get("exit_code").and_then(|x| x.as_i64());
                    out.push(json!({ "type": "tool_call", "payload": {
                        "name": "shell", "input": { "command": cmd }, "exit_code": exit } }));
                    if exit.map(|c| c != 0).unwrap_or(false) {
                        *saw_error = true;
                    }
                }
                Some("file_change") => {
                    out.push(
                        json!({ "type": "file_change", "payload": { "kind": item.get("kind") } }),
                    );
                }
                Some("reasoning") => {
                    // Token-heavy chain-of-thought: surface as a dim log only.
                    let text = item.get("text").and_then(|x| x.as_str()).unwrap_or("");
                    if !text.is_empty() {
                        out.push(json!({ "type": "log", "payload": { "text": text } }));
                    }
                }
                _ => {}
            }
        }
        "turn.completed" => {
            let mut tokens = 0u64;
            if let Some(usage) = v.get("usage") {
                for k in [
                    "input_tokens",
                    "output_tokens",
                    "cached_input_tokens",
                    "reasoning_output_tokens",
                ] {
                    tokens += usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                }
            }
            if let Some(cost_usd) = v.get("cost_usd").and_then(|x| x.as_f64()) {
                out.push(json!({ "type": "progress", "payload": {
                    "tokens": tokens, "cost_cents": (cost_usd * 100.0).round() as u64 } }));
            } else if tokens > 0 {
                out.push(json!({ "type": "progress", "payload": { "tokens": tokens } }));
            }
        }
        "error" => {
            *saw_error = true;
            let msg = v.get("message").and_then(|x| x.as_str()).unwrap_or(line);
            out.push(json!({ "type": "error", "payload": { "message": msg } }));
        }
        "thread.started" | "turn.started" | "item.started" | "item.delta" => {}
        _ => out.push(json!({ "type": "log", "payload": { "text": line } })),
    }
    out
}

fn main() {
    let mut prompt = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--version" | "-v" => {
                println!("adapter-codex {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--prompt" => prompt = args.next().unwrap_or_default(),
            _ => {}
        }
    }

    let bin = std::env::var("AGENTGRID_CODEX_BIN").unwrap_or_else(|_| "codex".into());
    let unsafe_unattended = agentgrid_adapters::unsafe_unattended_from_env();
    agentgrid_adapters::warn_unsafe("adapter-codex", unsafe_unattended, false);
    let args = codex_args(&prompt, unsafe_unattended);
    let mut cmd = Command::new(&bin);
    cmd.args(&args);
    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("adapter-codex: failed to spawn {bin}: {e}");
            std::process::exit(127);
        }
    };

    let stderr = child.stderr.take().unwrap();
    let err_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            eprintln!("{line}");
        }
    });

    let stdout = child.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    let mut saw_error = false;
    let mut last_msg = String::new();
    for line in reader.lines().map_while(Result::ok) {
        for ev in translate(&line, &mut saw_error, &mut last_msg) {
            emit_event(ev);
        }
    }

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("adapter-codex: wait for {bin} failed: {e}");
            std::process::exit(1);
        }
    };
    let _ = err_thread.join();
    let code = status.code().unwrap_or(1);
    // Terminal result event for the stream; codex has no `result` line, so we
    // synthesize one from the last agent message (empty is fine).
    emit_event(json!({ "type": "result", "payload": { "text": last_msg } }));
    std::process::exit(if saw_error && code == 0 { 1 } else { code });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(events: &[serde_json::Value]) -> Vec<&str> {
        events
            .iter()
            .map(|e| e.get("type").and_then(|t| t.as_str()).unwrap_or(""))
            .collect()
    }

    #[test]
    fn args_safe_default_workspace_write() {
        let a = codex_args("do it", false);
        let j = a.join(" ");
        assert!(j.contains("exec --json"));
        assert!(j.contains("--sandbox workspace-write"));
        assert!(!j.contains("dangerously"));
    }

    #[test]
    fn args_unsafe_bypasses_sandbox() {
        let a = codex_args("do it", true);
        assert!(a
            .iter()
            .any(|x| x == "--dangerously-bypass-approvals-and-sandbox"));
        assert!(!a.contains(&"--sandbox".to_string()));
    }

    #[test]
    fn translate_agent_message_and_plan() {
        let mut err = false;
        let mut last = String::new();
        let line = json!({"type":"item.completed","item":{"type":"agent_message",
            "text":"work plan:\n```plan\n- id: w\n```"}})
        .to_string();
        let evs = translate(&line, &mut err, &mut last);
        assert_eq!(types(&evs), vec!["plan", "log"]);
        assert!(!last.is_empty());
    }

    #[test]
    fn translate_command_execution_error_marks_error() {
        let mut err = false;
        let mut last = String::new();
        let line = json!({"type":"item.completed","item":{"type":"command_execution",
            "command":"bash -lc false","exit_code":1}})
        .to_string();
        let evs = translate(&line, &mut err, &mut last);
        assert_eq!(types(&evs), vec!["tool_call"]);
        assert!(err);
    }

    #[test]
    fn translate_turn_usage_emits_progress() {
        let mut err = false;
        let mut last = String::new();
        let line = json!({"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":25}})
            .to_string();
        let evs = translate(&line, &mut err, &mut last);
        assert_eq!(types(&evs), vec!["progress"]);
        assert_eq!(evs[0]["payload"]["tokens"], 125);
    }

    #[test]
    fn translate_error_line_flags_error() {
        let mut err = false;
        let mut last = String::new();
        let evs = translate(r#"{"type":"error","message":"boom"}"#, &mut err, &mut last);
        assert_eq!(types(&evs), vec!["error"]);
        assert!(err);
    }

    #[test]
    fn translate_unparseable_line_becomes_log() {
        let mut err = false;
        let mut last = String::new();
        let evs = translate("not json", &mut err, &mut last);
        assert_eq!(types(&evs), vec!["log"]);
    }
}
