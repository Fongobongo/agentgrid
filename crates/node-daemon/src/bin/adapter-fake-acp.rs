//! Test-only ACP agent: speaks minimal JSON-RPC 2.0 over stdio so the node
//! daemon's `drive_acp_session` can be exercised end-to-end without a real
//! agent. Not shipped; `CARGO_BIN_EXE_adapter-fake-acp` is referenced by the
//! node-daemon integration tests.

use serde_json::{json, Value};
use std::io::{BufRead, BufWriter, Write};

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let reader = std::io::BufReader::new(stdin.lock());
    let session_id = "sess-fake-1";

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "protocol_version": "0.1", "capabilities": {}, "client": {} }
                });
                writeln!(out, "{}", resp).ok();
            }
            "session/new" => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "session_id": session_id }
                });
                writeln!(out, "{}", resp).ok();
            }
            "session/prompt" => {
                // Test mode (Stage 4 integration): record the received prompt
                // (after the node appends skills/context blocks) to a file
                // so the test can assert the skills block was injected.
                if let Some(p) = std::env::var_os("AG_FAKE_RECORD_PROMPT") {
                    if let Some(prompt) = v
                        .get("params")
                        .and_then(|p| p.get("prompt"))
                        .and_then(|p| p.as_str())
                    {
                        let _ = std::fs::write(&p, prompt);
                    }
                }
                // Test mode: hang mid-frame. Write the start of a JSON-RPC
                // `session/update` line with no terminating newline and block,
                // simulating an ACP subprocess that dies mid-frame (truncated
                // JSON). The block is interruptible: a background thread reads
                // stdin and flips `cancelled` when it sees `session/cancel`, so
                // the node's cancel path (`session/cancel` → reap) resolves
                // this branch instead of hanging forever.
                if std::env::var_os("AG_FAKE_HANG").is_some() {
                    let partial = r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"type":"progress","text":"thi"#;
                    out.write_all(partial.as_bytes()).ok();
                    out.flush().ok();
                    // A peer-thread that reads stdin and flags cancel.
                    use std::sync::atomic::{AtomicBool, Ordering};
                    use std::sync::Arc;
                    let cancelled = Arc::new(AtomicBool::new(false));
                    let cancelled2 = cancelled.clone();
                    std::thread::spawn(move || {
                        let stdin = std::io::stdin();
                        let r = std::io::BufReader::new(stdin.lock());
                        for line in r.lines() {
                            let line = match line {
                                Ok(l) => l,
                                Err(_) => break,
                            };
                            if line.contains("session/cancel") {
                                cancelled2.store(true, Ordering::SeqCst);
                            }
                        }
                    });
                    // Spin until cancelled, or 90s fallback (the node tears us
                    // down long before that).
                    let started = std::time::Instant::now();
                    while !cancelled.load(Ordering::SeqCst)
                        && started.elapsed() < std::time::Duration::from_secs(90)
                    {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    // Respond to the session/cancel with an empty result so
                    // the node's `session_cancel` RPC completes and reaps us.
                    let resp = json!({ "jsonrpc": "2.0", "id": Value::Null, "result": {} });
                    writeln!(out, "{}", resp).ok();
                    out.flush().ok();
                    break;
                }
                // Emit two updates, then the prompt result.
                let u1 = json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": { "update": { "type": "progress", "text": "thinking" } }
                });
                let u2 = json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": { "update": { "type": "tool_call", "tool": "bash", "input": "echo hi" } }
                });
                writeln!(out, "{}", u1).ok();
                writeln!(out, "{}", u2).ok();
                out.flush().ok();
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "text": "done" }
                });
                writeln!(out, "{}", resp).ok();
            }
            "session/cancel" => {
                let resp = json!({ "jsonrpc": "2.0", "id": id, "result": {} });
                writeln!(out, "{}", resp).ok();
                out.flush().ok();
                break;
            }
            _ => {
                let resp = json!({ "jsonrpc": "2.0", "id": id, "result": {} });
                writeln!(out, "{}", resp).ok();
            }
        }
        out.flush().ok();
    }
}
