//! Real adapter wrapper for OpenCode (Stage 3.2).
//!
//! Launches the `opencode` CLI headless in JSON-event mode
//! (`opencode run --format json`) and translates its structured events into
//! the agentgrid event contract (NDJSON on stdout). Unknown event types are
//! ignored; the daemon always preserves the raw stdout as
//! `agent-raw-output.log` (Stage 3.1), so a future opencode output-format
//! change cannot lose information.
//!
//! Invocation contract (matches the daemon): `--prompt "<text>"`, run with
//! cwd = attempt worktree. API keys are forwarded by the daemon through
//! `AGENTGRID_ADAPTER_ENV` (e.g. `GEMINI_API_KEY`).
//!
//! Configuration (all optional env):
//!   AGENTGRID_OPENCODE_BIN   binary name (default `opencode`)
//!   AGENTGRID_OPENCODE_MODEL model `provider/model` (else opencode's default)
//!   AGENTGRID_OPENCODE_AUTO  set to `0`/`false` to disable `--auto`
//!                             (auto-approve permissions so the agent can act)

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::json;

fn emit_event(ev: serde_json::Value) {
    let line = serde_json::to_string(&ev).unwrap();
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Translate one `opencode run --format json` line into agentgrid events.
fn translate(line: &str, saw_error: &mut bool) -> Vec<serde_json::Value> {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
    let part = v.get("part");
    match t {
        "text" => {
            if let Some(text) = part.and_then(|p| p.get("text")).and_then(|x| x.as_str()) {
                if !text.is_empty() {
                    return vec![json!({ "type": "log", "payload": { "text": text } })];
                }
            }
            Vec::new()
        }
        "tool_use" => {
            let mut out = Vec::new();
            if let Some(p) = part {
                let name = p.get("tool").and_then(|x| x.as_str()).unwrap_or("");
                let call_id = p.get("callID").and_then(|x| x.as_str()).unwrap_or("");
                let input = p
                    .get("state")
                    .and_then(|s| s.get("input"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                out.push(json!({
                    "type": "tool_call",
                    "payload": { "name": name, "callID": call_id, "input": input }
                }));
                // The tool_use part carries the full lifecycle; emit the result
                // once the call has completed and produced output.
                if let Some(state) = p.get("state") {
                    if state.get("status").and_then(|x| x.as_str()) == Some("completed") {
                        let result = state
                            .get("output")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        out.push(json!({
                            "type": "tool",
                            "payload": { "name": name, "callID": call_id, "result": result }
                        }));
                    }
                }
            }
            out
        }
        "error" => {
            *saw_error = true;
            let msg = v
                .get("error")
                .and_then(|e| e.get("data"))
                .and_then(|d| d.get("message"))
                .and_then(|m| m.as_str())
                .or_else(|| part.and_then(|p| p.get("text")).and_then(|x| x.as_str()))
                .unwrap_or("opencode error");
            vec![json!({ "type": "error", "payload": { "text": msg } })]
        }
        _ => Vec::new(),
    }
}

fn main() {
    let mut prompt = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--version" | "-v" => {
                println!("adapter-opencode {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--prompt" => prompt = args.next().unwrap_or_default(),
            _ => {}
        }
    }

    let bin = std::env::var("AGENTGRID_OPENCODE_BIN").unwrap_or_else(|_| "opencode".into());
    let model = std::env::var("AGENTGRID_OPENCODE_MODEL")
        .or_else(|_| std::env::var("AGENTGRID_MODEL"))
        .ok();
    let auto = std::env::var("AGENTGRID_OPENCODE_AUTO")
        .map(|v| v != "0" && v != "false")
        .unwrap_or(true);

    let mut cmd = Command::new(&bin);
    cmd.arg("run").arg("--format").arg("json");
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }
    if auto {
        cmd.arg("--auto");
    }
    cmd.arg(&prompt);

    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("adapter-opencode: failed to spawn {bin}: {e}");
            std::process::exit(127);
        }
    };

    // Drain stderr so a full pipe cannot block the child.
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
    fn translate_ignores_unparseable_and_unknown() {
        let mut err = false;
        assert!(translate("not json", &mut err).is_empty());
        assert!(translate(r#"{"type":"step_start","part":{}}"#, &mut err).is_empty());
        assert!(!err);
    }

    #[test]
    fn translate_text_becomes_log() {
        let mut err = false;
        let line = r#"{"type":"text","part":{"type":"text","text":"hello"}}"#;
        let evs = translate(line, &mut err);
        assert_eq!(types(&evs), vec!["log"]);
        assert_eq!(evs[0]["payload"]["text"], "hello");
        assert!(!err);
    }

    #[test]
    fn translate_tool_use_emits_call_and_result() {
        let mut err = false;
        let line = r#"{"type":"tool_use","part":{"type":"tool","tool":"bash","callID":"abc","state":{"status":"completed","input":{"command":"echo hi"},"output":"hi\n"}}}"#;
        let evs = translate(line, &mut err);
        assert_eq!(types(&evs), vec!["tool_call", "tool"]);
        assert_eq!(evs[0]["payload"]["name"], "bash");
        assert_eq!(evs[0]["payload"]["callID"], "abc");
        assert_eq!(evs[1]["payload"]["result"], "hi\n");
        assert!(!err);
    }

    #[test]
    fn translate_error_marks_error() {
        let mut err = false;
        let line = r#"{"type":"error","error":{"name":"APIError","data":{"message":"boom"}}}"#;
        let evs = translate(line, &mut err);
        assert_eq!(types(&evs), vec!["error"]);
        assert!(err);
    }

    // Real-adapter smoke test: requires the `opencode` CLI and a configured
    // model. Ignored by default; run manually or in nightly CI (spec: real-agent
    // tests need keys, so they are `#[ignore]`).
    #[test]
    #[ignore = "needs opencode CLI + AGENTGRID_OPENCODE_MODEL"]
    fn real_opencode_emits_events() {
        let bin = std::env::var("AGENTGRID_OPENCODE_BIN").unwrap_or_else(|_| "opencode".into());
        let model = match std::env::var("AGENTGRID_OPENCODE_MODEL") {
            Ok(m) => m,
            Err(_) => {
                eprintln!("AGENTGRID_OPENCODE_MODEL unset; skipping");
                return;
            }
        };
        let child = match Command::new(&bin)
            .arg("run")
            .arg("--format")
            .arg("json")
            .arg("--model")
            .arg(&model)
            .arg("--auto")
            .arg("reply with exactly the word: ok")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("cannot spawn {bin}: {e}; skipping");
                return;
            }
        };
        let mut saw_error = false;
        let mut all = Vec::new();
        let reader = BufReader::new(child.stdout.unwrap());
        for line in reader.lines().map_while(Result::ok) {
            all.extend(translate(&line, &mut saw_error));
        }
        assert!(!all.is_empty(), "opencode produced no translatable events");
        assert!(
            all.iter()
                .any(|e| e.get("type").and_then(|t| t.as_str()) == Some("text")
                    || e.get("type").and_then(|t| t.as_str()) == Some("error")),
            "opencode stream should yield text or an error event"
        );
    }
}
