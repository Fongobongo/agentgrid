//! Real adapter wrapper for the Pi coding agent (`pi`).
//!
//! Launches `pi --mode json -p "<prompt>"` and translates Pi's documented
//! `JsonAgentSessionEvent` stream (see `docs/json.md` in the Pi package)
//! into the agentgrid event contract (NDJSON on stdout). Unknown lines fall
//! back to raw `log` events, so Pi CLI format drift cannot break the
//! pipeline — the daemon also keeps the raw stdout as
//! `agent-raw-output.log`.
//!
//! Invocation contract (matches the daemon): `--prompt "<text>"`, cwd =
//! attempt worktree. Model/provider/api key come from Pi's own env /
//! config (`PI_...`, provider env vars); optionally `AGENTGRID_ADAPTER_ENV`
//! sets `PI_MODEL=<provider/model>` map — here surfaced as a plain
//! `--model` flag via `AGENTGRID_PI_MODEL`.
//!
//! Configuration:
//!   AGENTGRID_PI_BIN     binary (default `pi`)
//!   AGENTGRID_PI_MODEL   `--model` selector (e.g. "github-copilot/gpt-5.1")
//!   AGENTGRID_PI_THINK   `--thinking <level>` (optional)
//!
//! Note: Pi has no "dangerously bypass" switch in `--print` mode — it is a
//! coding agent with tools enabled; run it on sandboxed nodes with network
//! policy like any other real adapter. The unsafe/unattended banner is
//! still emitted for parity with the other adapters.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::json;

fn emit_event(ev: serde_json::Value) {
    let line = serde_json::to_string(&ev).unwrap();
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn pi_args(prompt: &str) -> Vec<String> {
    let mut args = vec!["--mode".into(), "json".into(), "-p".into()];
    if let Ok(m) = std::env::var("AGENTGRID_PI_MODEL") {
        let m = m.trim().to_string();
        if !m.is_empty() {
            args.push("--model".into());
            args.push(m);
        }
    }
    if let Ok(t) = std::env::var("AGENTGRID_PI_THINK") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            args.push("--thinking".into());
            args.push(t);
        }
    }
    args.push(prompt.to_string());
    args
}

/// Text content of an assistant message (concatenate content blocks of the
/// form {type:"text", text:"..."}).
fn assistant_text(msg: &serde_json::Value) -> String {
    msg.get("content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|b| b.get("type").and_then(|x| x.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|x| x.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Running aggregate over agent_end usage (when present).
#[derive(Default)]
struct Usage {
    total: u64,
}

fn add_usage(u: &mut Usage, message: &serde_json::Value) {
    if let Some(usage) = message.get("usage") {
        u.total += usage
            .get("totalTokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
    }
}

/// Translate one Pi `--mode json` event line into agentgrid events.
fn translate(line: &str, saw_error: &mut bool, usage: &mut Usage) -> Vec<serde_json::Value> {
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
        "tool_execution_start" => {
            let name = v.get("toolName").and_then(|x| x.as_str()).unwrap_or("");
            let input = v.get("args").cloned().unwrap_or(serde_json::Value::Null);
            out.push(json!({ "type": "tool_call", "payload": { "name": name, "input": input } }));
        }
        "tool_execution_end" => {
            let name = v.get("toolName").and_then(|x| x.as_str()).unwrap_or("");
            let is_err = v.get("isError").and_then(|x| x.as_bool()).unwrap_or(false);
            if is_err {
                *saw_error = true;
            }
            out.push(json!({ "type": "tool_call", "payload": {
                "name": name, "result": v.get("result").cloned().unwrap_or(serde_json::Value::Null),
                "error": is_err } }));
        }
        "message_end" | "message_update" => {
            if let Some(m) = v.get("message") {
                if m.get("role").and_then(|x| x.as_str()) == Some("assistant") {
                    let text = assistant_text(m);
                    if !text.is_empty() {
                        for plan in agentgrid_adapters::plan_blocks(&text) {
                            out.push(json!({ "type": "plan", "payload": { "text": plan } }));
                        }
                        out.push(json!({ "type": "log", "payload": { "text": text } }));
                    }
                }
            }
        }
        "agent_end" => {
            // Emit a result event with the last assistant text + aggregate
            // usage into a single progress event.
            if let Some(msgs) = v.get("messages").and_then(|x| x.as_array()) {
                let mut answer = String::new();
                for m in msgs.iter().rev() {
                    if m.get("role").and_then(|x| x.as_str()) == Some("assistant") {
                        add_usage(usage, m);
                        if answer.is_empty() {
                            let t = assistant_text(m);
                            if !t.is_empty() {
                                answer = t;
                            }
                        }
                    }
                }
                if usage.total > 0 {
                    out.push(json!({ "type": "progress", "payload": { "tokens": usage.total } }));
                }
                out.push(json!({ "type": "result", "payload": { "text": answer } }));
            }
        }
        "compaction_start" | "compaction_end" | "teleport" | "turn_start" | "turn_end"
        | "agent_start" | "queue_update" | "message_start" | "auto_retry_start"
        | "auto_retry_end" => {}
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
                println!("adapter-pi {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--prompt" => prompt = args.next().unwrap_or_default(),
            _ => {}
        }
    }

    let bin = std::env::var("AGENTGRID_PI_BIN").unwrap_or_else(|_| "pi".into());
    let unsafe_unattended = agentgrid_adapters::unsafe_unattended_from_env();
    agentgrid_adapters::warn_unsafe("adapter-pi", unsafe_unattended, false);
    let args = pi_args(&prompt);
    let mut cmd = Command::new(&bin);
    cmd.args(&args);
    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("adapter-pi: failed to spawn {bin}: {e}");
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
    let mut usage = Usage::default();
    for line in reader.lines().map_while(Result::ok) {
        for ev in translate(&line, &mut saw_error, &mut usage) {
            emit_event(ev);
        }
    }

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("adapter-pi: wait for {bin} failed: {e}");
            std::process::exit(1);
        }
    };
    let _ = err_thread.join();
    let code = status.code().unwrap_or(1);
    std::process::exit(if saw_error && code == 0 { 1 } else { code });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(evs: &[serde_json::Value]) -> Vec<&str> {
        evs.iter()
            .map(|e| e.get("type").and_then(|t| t.as_str()).unwrap_or(""))
            .collect()
    }

    #[test]
    fn translate_tool_execution_events() {
        let mut err = false;
        let mut u = Usage::default();
        let start = json!({"type":"tool_execution_start","toolCallId":"a","toolName":"write",
                           "args":{"path":"/tmp/x","text":"hi"}})
        .to_string();
        let evs = translate(&start, &mut err, &mut u);
        assert_eq!(types(&evs), vec!["tool_call"]);
        assert_eq!(evs[0]["payload"]["name"], "write");
        let end = json!({"type":"tool_execution_end","toolCallId":"a","toolName":"write",
                         "result":{"ok":true},"isError":false})
        .to_string();
        let evs = translate(&end, &mut err, &mut u);
        assert_eq!(types(&evs), vec!["tool_call"]);
        assert!(!err);
        let bad = json!({"type":"tool_execution_end","toolCallId":"b","toolName":"bash",
                         "result":{},"isError":true})
        .to_string();
        translate(&bad, &mut err, &mut u);
        assert!(err);
    }

    #[test]
    fn translate_assistant_message_end() {
        let mut err = false;
        let mut u = Usage::default();
        let line = json!({"type":"message_end","message":{"role":"assistant",
            "content":[{"type":"text","text":"done"},{"type":"tool_use","id":"x","name":"bash","input":{}}],
            "usage":{"totalTokens":120}}})
        .to_string();
        let evs = translate(&line, &mut err, &mut u);
        assert_eq!(types(&evs), vec!["log"]);
        assert_eq!(evs[0]["payload"]["text"], "done");
    }

    #[test]
    fn agent_end_carries_usage_and_result() {
        let mut err = false;
        let mut u = Usage::default();
        let line = json!({"type":"agent_end","messages":[
            {"role":"user","content":[{"type":"text","text":"hi"}]},
            {"role":"assistant","content":[{"type":"text","text":"all good"}],
             "usage":{"totalTokens":260}}
        ]})
        .to_string();
        let evs = translate(&line, &mut err, &mut u);
        assert_eq!(types(&evs), vec!["progress", "result"]);
        assert_eq!(evs[0]["payload"]["tokens"], 260);
        assert_eq!(evs[1]["payload"]["text"], "all good");
    }

    #[test]
    fn unknown_line_stays_a_log_event() {
        let mut err = false;
        let mut u = Usage::default();
        let evs = translate("not json", &mut err, &mut u);
        assert_eq!(types(&evs), vec!["log"]);
    }

    #[test]
    fn plan_fences_surface() {
        let mut err = false;
        let mut u = Usage::default();
        let line = json!({"type":"message_end","message":{"role":"assistant",
            "content":[{"type":"text","text":"```plan\n- id: w\n  prompt: work\n```"}]}})
        .to_string();
        let evs = translate(&line, &mut err, &mut u);
        assert!(evs.iter().any(|e| e["type"] == "plan"));
    }
}
