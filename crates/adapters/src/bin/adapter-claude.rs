//! Real adapter wrapper for Claude Code (Stage 3.2, ADR #12).
//!
//! Launches the `claude` CLI headless in `stream-json` mode and translates its
//! output into the agentgrid event contract (NDJSON on stdout). Unknown lines
//! fall back to a raw `log` event, so a future `claude` output-format change
//! cannot break the pipeline — the daemon also preserves the raw stdout as the
//! `agent-raw-output.log` artifact.
//!
//! Invocation contract (matches the daemon): `--prompt "<text>"`, run with cwd
//! = attempt worktree. API key is supplied via env (e.g. `ANTHROPIC_API_KEY`)
//! forwarded by the daemon through `AGENTGRID_ADAPTER_ENV`.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::json;

fn emit_event(ev: serde_json::Value) {
    let line = serde_json::to_string(&ev).unwrap();
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Translate one `claude` `stream-json` line into agentgrid events.
fn translate(line: &str, saw_error: &mut bool) -> Vec<serde_json::Value> {
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
        "assistant" => {
            if let Some(arr) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for block in arr {
                    match block.get("type").and_then(|x| x.as_str()) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(|x| x.as_str()) {
                                out.push(json!({ "type": "log", "payload": { "text": text } }));
                            }
                        }
                        Some("tool_use") => {
                            let name = block.get("name").and_then(|x| x.as_str()).unwrap_or("");
                            let input = block.get("input").cloned().unwrap_or(serde_json::Value::Null);
                            out.push(json!({ "type": "tool_call", "payload": { "name": name, "input": input } }));
                        }
                        _ => {}
                    }
                }
            }
        }
        "user" => {
            if let Some(content) = v.get("content") {
                if let Some(arr) = content.as_array() {
                    for block in arr {
                        if block.get("type").and_then(|x| x.as_str()) == Some("tool_result") {
                            let res = block.get("content").cloned().unwrap_or(serde_json::Value::Null);
                            out.push(json!({ "type": "tool", "payload": { "result": res } }));
                        }
                    }
                } else if let Some(s) = content.as_str() {
                    if !s.is_empty() {
                        out.push(json!({ "type": "log", "payload": { "text": s } }));
                    }
                }
            }
        }
        "result" => {
            if v.get("is_error").and_then(|x| x.as_bool()) == Some(true) {
                *saw_error = true;
            }
            let text = v.get("result").and_then(|x| x.as_str()).unwrap_or("");
            out.push(json!({ "type": "result", "payload": { "text": text } }));
        }
        // `system` and anything else: keep as a raw log line.
        _ => out.push(json!({ "type": "log", "payload": { "text": line } })),
    }
    out
}

fn main() {
    let mut prompt = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--prompt" {
            prompt = args.next().unwrap_or_default();
        }
    }

    let bin = std::env::var("AGENTGRID_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    let mut child = match Command::new(&bin)
        .arg("-p")
        .arg(&prompt)
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--dangerously-skip-permissions")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("adapter-claude: failed to spawn {bin}: {e}");
            std::process::exit(127);
        }
    };

    // Drain stderr in a thread so it cannot block the child on a full pipe.
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
    for line in reader.lines().map_while(Result::ok) {
        for ev in translate(&line, &mut saw_error) {
            emit_event(ev);
        }
    }

    let status = child.wait().unwrap();
    let _ = err_thread.join();
    let code = status.code().unwrap_or(1);
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
    fn translate_unparseable_line_becomes_log() {
        let mut err = false;
        let evs = translate("not json at all", &mut err);
        assert_eq!(types(&evs), vec!["log"]);
        assert!(!err);
    }

    #[test]
    fn translate_assistant_text_and_tool_use() {
        let mut err = false;
        let line = json!({
            "type": "assistant",
            "message": {
                "content": [
                    { "type": "text", "text": "editing file" },
                    { "type": "tool_use", "name": "Edit", "input": { "file": "a.rs" } }
                ]
            }
        })
        .to_string();
        let evs = translate(&line, &mut err);
        assert_eq!(types(&evs), vec!["log", "tool_call"]);
        assert_eq!(evs[1]["payload"]["name"], "Edit");
        assert!(!err);
    }

    #[test]
    fn translate_result_marks_error() {
        let mut err = false;
        let line = json!({ "type": "result", "result": "done", "is_error": true })
            .to_string();
        let evs = translate(&line, &mut err);
        assert_eq!(types(&evs), vec!["result"]);
        assert!(err);
    }
}
