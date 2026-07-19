//! Mock adapter: deterministic, no LLM required. Reads the prompt from
//! `--prompt "<text>"` (or `AGENTGRID_PROMPT`) and interprets embedded
//! command lines:
//!   sleep:<seconds>   - block (for cancel/timeout tests)
//!   write:<file>:<content> - write a file into the cwd (the attempt worktree)
//!   fail:<exit-code>  - finish with a non-zero exit code
//!   spam:<n>          - emit n log lines (streaming/buffer tests)
//! Any other line is logged as a note. Emits a final `result` event.

use std::env;
use std::fs;
use std::io::Write;
use std::time::Duration;

use serde_json::json;

fn emit(ty: &str, payload: serde_json::Value) {
    let line = json!({ "type": ty, "payload": payload });
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{}", serde_json::to_string(&line).unwrap());
    let _ = out.flush();
}

fn parse_prompt() -> String {
    let mut args = env::args().skip(1);
    let mut prompt = String::new();
    while let Some(a) = args.next() {
        if a == "--prompt" {
            prompt = args.next().unwrap_or_default();
        }
    }
    if prompt.is_empty() {
        prompt = env::var("AGENTGRID_PROMPT").unwrap_or_default();
    }
    prompt
}

fn main() {
    let prompt = parse_prompt();
    emit(
        "log",
        json!({ "text": format!("mock adapter started (attempt {})", env::var("AGENTGRID_ATTEMPT_ID").unwrap_or_default()) }),
    );

    let mut exit_code: i32 = 0;
    for raw in prompt.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(secs) = line.strip_prefix("sleep:") {
            let s: f64 = secs.parse().unwrap_or(0.0);
            emit("log", json!({ "text": format!("sleeping {s}s") }));
            std::thread::sleep(Duration::from_secs_f64(s));
        } else if let Some(rest) = line.strip_prefix("write:") {
            let mut parts = rest.splitn(2, ':');
            let file = parts.next().unwrap_or("").to_string();
            let content = parts.next().unwrap_or("").to_string();
            match fs::write(&file, &content) {
                Ok(()) => emit(
                    "file_change",
                    json!({ "path": file, "bytes": content.len() }),
                ),
                Err(e) => emit(
                    "error",
                    json!({ "text": format!("write {file} failed: {e}") }),
                ),
            }
        } else if let Some(code) = line.strip_prefix("fail:") {
            exit_code = code.parse().unwrap_or(1);
            emit(
                "error",
                json!({ "text": format!("forced failure exit_code={exit_code}") }),
            );
        } else if let Some(n) = line.strip_prefix("spam:") {
            let n: usize = n.parse().unwrap_or(0);
            for i in 0..n {
                emit("log", json!({ "text": format!("spam line {i}") }));
            }
        } else {
            emit("log", json!({ "text": format!("note: {line}") }));
        }
    }

    // Echo the last user line back as the result text so the chat loop has a
    // readable answer (mock has no LLM; real adapters emit their own text).
    let answer = prompt
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_string();
    emit("result", json!({ "exit_code": exit_code, "text": answer }));
    std::process::exit(exit_code);
}
