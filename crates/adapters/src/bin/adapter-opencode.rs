//! Real adapter wrapper for OpenCode (Stage 3.2).
//!
//! Launches the `opencode` CLI headless in JSON-event mode
//! (`opencode run --format json`) and translates its structured events into
//! the agentgrid event contract (NDJSON on stdout). Unknown event types are
//! ignored; the daemon always preserves the raw stdout as
//! `agent-raw-output.log` (Stage 3.1), so a future opencode output-format
//! change cannot lose information.
//!
//! Verified against both release lines: v1.19.5 and v2.2.1. They share the
//! `{ type, timestamp, sessionID, part }` envelope and the `text` /
//! `tool_use` / `step_finish` event names, but differ in three fields that
//! silently degrade rather than fail, so each is read with a fallback:
//!
//! | field        | v1                     | v2                          |
//! |--------------|------------------------|-----------------------------|
//! | call id      | `part.callID`          | `part.id` (v2's `partID` is the part) |
//! | token total  | `part.tokens.total`    | absent — only the buckets    |
//! | error text   | `error.data.message`   | `error.message`              |
//!
//! Reading only the v1 spelling would give empty call ids (so a tool call and
//! its result can't be correlated), zero tokens (so `max_tokens` never fires),
//! and the generic "opencode error".
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

/// Sum the token buckets of a `step_finish` part.
///
/// v1's `total` equals exactly `input + output + reasoning + cache.read +
/// cache.write`, so this reproduces it for v2, which reports only the buckets.
fn token_sum(tokens: &serde_json::Value) -> u64 {
    let bucket = |key: &str| tokens.get(key).and_then(|x| x.as_u64()).unwrap_or(0);
    let cache = |key: &str| {
        tokens
            .get("cache")
            .and_then(|c| c.get(key))
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
    };
    bucket("input") + bucket("output") + bucket("reasoning") + cache("read") + cache("write")
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
            let mut out = Vec::new();
            if let Some(text) = part.and_then(|p| p.get("text")).and_then(|x| x.as_str()) {
                if !text.is_empty() {
                    out.push(json!({ "type": "log", "payload": { "text": text } }));
                }
                // Stage 13 plan approval: an architect instructed to fence its
                // machine-readable plan in ```plan gets it surfaced here.
                for plan in agentgrid_adapters::plan_blocks(text) {
                    out.push(json!({ "type": "plan", "payload": { "text": plan } }));
                }
            }
            out
        }
        "tool_use" => {
            let mut out = Vec::new();
            if let Some(p) = part {
                let name = p.get("tool").and_then(|x| x.as_str()).unwrap_or("");
                // v1 puts the provider call id in `callID` and the part id in
                // `id`; v2 drops `callID` and uses `id` for the call id (the
                // part id moved to `partID`). Prefer `callID` so v1 keeps its
                // provider id, and fall back to `id` for v2.
                let call_id = p
                    .get("callID")
                    .or_else(|| p.get("id"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
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
                            "type": "tool_call",
                            "payload": { "name": name, "callID": call_id, "result": result }
                        }));
                    }
                }
            }
            out
        }
        "step_finish" => {
            // opencode reports per-step usage here; surface it as `progress`
            // (stored as a `metric` event) so workflow token budgets can fire.
            if let Some(tokens) = part.and_then(|p| p.get("tokens")) {
                // v1 precomputes `total`; v2 dropped it and reports only the
                // buckets. Without the sum a v2 run reports zero tokens and
                // `max_tokens` never fires — a silent hole, not a crash.
                let total = tokens
                    .get("total")
                    .and_then(|x| x.as_u64())
                    .unwrap_or_else(|| token_sum(tokens));
                let cost_cents = part
                    .and_then(|p| p.get("cost"))
                    .and_then(|x| x.as_f64())
                    .map(|c| (c * 100.0).round() as u64)
                    .unwrap_or(0);
                return vec![json!({
                    "type": "progress",
                    "payload": { "tokens": total, "cost_cents": cost_cents }
                })];
            }
            Vec::new()
        }
        "error" => {
            *saw_error = true;
            // v1 nests the text under `error.data.message`; v2 flattens it to
            // `error.message`. Without the second path a v2 failure surfaces as
            // the generic "opencode error" and the operator loses the reason.
            let msg = v
                .get("error")
                .and_then(|e| e.get("data"))
                .and_then(|d| d.get("message"))
                .and_then(|m| m.as_str())
                .or_else(|| {
                    v.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                })
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
    // Hardening P0 (unsafe adapter defaults): `--auto` is OFF by default; only
    // `AGENTGRID_UNSAFE_UNATTENDED=1` or a legacy `AGENTGRID_OPENCODE_AUTO` knob
    // turns it on.
    let unsafe_unattended = agentgrid_adapters::unsafe_unattended_from_env();
    let auto = agentgrid_adapters::opencode_auto(unsafe_unattended);
    agentgrid_adapters::warn_unsafe(
        "adapter-opencode",
        unsafe_unattended,
        auto && !unsafe_unattended,
    );

    let mut cmd = Command::new(&bin);
    cmd.arg("run").arg("--format").arg("json");
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }
    if auto {
        cmd.arg("--auto");
    }
    cmd.arg(&prompt);

    // `stdin(Stdio::null())` is not cosmetic: opencode reads stdin when it is
    // a pipe and blocks until EOF. The daemon spawns adapters with an inherited
    // stdin that never closes, so without this the agent hangs forever on a
    // silent stream — verified against v2.2.1 (open pipe: no output, timeout;
    // /dev/null: normal run).
    let mut child = match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
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

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("adapter-opencode: wait for {bin} failed: {e}");
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
    fn translate_text_with_plan_fence_emits_plan_event() {
        let mut err = false;
        let text = "ok\n```plan\n- id: w\n  prompt: work\n  role: worker\n```\n";
        let line =
            serde_json::json!({"type":"text","part":{"type":"text","text":text}}).to_string();
        let evs = translate(&line, &mut err);
        assert_eq!(types(&evs), vec!["log", "plan"]);
        assert_eq!(
            evs[1]["payload"]["text"],
            "- id: w\n  prompt: work\n  role: worker"
        );
        assert!(!err);
    }

    #[test]
    fn translate_tool_use_emits_call_and_result() {
        let mut err = false;
        let line = r#"{"type":"tool_use","part":{"type":"tool","tool":"bash","callID":"abc","state":{"status":"completed","input":{"command":"echo hi"},"output":"hi\n"}}}"#;
        let evs = translate(line, &mut err);
        assert_eq!(types(&evs), vec!["tool_call", "tool_call"]);
        assert_eq!(evs[0]["payload"]["name"], "bash");
        assert_eq!(evs[0]["payload"]["callID"], "abc");
        assert_eq!(evs[1]["payload"]["result"], "hi\n");
        assert!(!err);
    }

    // v2.2.1 capture: no `callID`, the call id is `id`, and `partID` is the
    // part. Reading only `callID` yields "" and the tool call loses its
    // correlation with the result event.
    #[test]
    fn translate_tool_use_v2_falls_back_to_part_id() {
        let mut err = false;
        let line = r#"{"type":"tool_use","part":{"partID":"prt_186e","sessionID":"ses_1","messageID":"msg_1","type":"tool","id":"call_00_df8i5kvtb6x","tool":"write","state":{"status":"completed","input":{"path":"a.txt"},"output":"Created file successfully: a.txt"}}}"#;
        let evs = translate(line, &mut err);
        assert_eq!(types(&evs), vec!["tool_call", "tool_call"]);
        assert_eq!(evs[0]["payload"]["callID"], "call_00_df8i5kvtb6x");
        assert_eq!(evs[0]["payload"]["name"], "write");
        assert_eq!(
            evs[1]["payload"]["result"],
            "Created file successfully: a.txt"
        );
        assert!(!err);
    }

    // v1.19.5 sends both fields, and there `id` is the *part* id. Preferring
    // `id` would replace a provider call id with a `prt_…` one.
    #[test]
    fn translate_tool_use_prefers_v1_call_id_over_part_id() {
        let mut err = false;
        let line = r#"{"type":"tool_use","part":{"callID":"chatcmpl-tool-89ad","id":"prt_186ee61d5001","type":"tool","tool":"write","state":{"status":"completed","input":{},"output":"ok"}}}"#;
        let evs = translate(line, &mut err);
        assert_eq!(evs[0]["payload"]["callID"], "chatcmpl-tool-89ad");
    }

    // v2.2.1 capture: `tokens` has no `total`. Reporting zero would keep a
    // token ceiling permanently unarmed.
    #[test]
    fn translate_step_finish_v2_sums_buckets_when_total_is_missing() {
        let mut err = false;
        let line = r#"{"type":"step_finish","part":{"type":"step-finish","reason":"tool-calls","cost":0,"tokens":{"input":120,"output":61,"reasoning":0,"cache":{"read":5248,"write":0}}}}"#;
        let evs = translate(line, &mut err);
        assert_eq!(types(&evs), vec!["progress"]);
        // 120 + 61 + 0 + 5248 + 0
        assert_eq!(evs[0]["payload"]["tokens"], 5429);
    }

    // v1's `total` is authoritative when present: it must win over the sum so
    // the adapter keeps reporting exactly what the CLI reported.
    #[test]
    fn translate_step_finish_prefers_reported_total() {
        let mut err = false;
        let line = r#"{"type":"step_finish","part":{"tokens":{"total":9999,"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}}}}"#;
        let evs = translate(line, &mut err);
        assert_eq!(evs[0]["payload"]["tokens"], 9999);
    }

    // v2.2.1 capture: the message is flat on `error`, not under `error.data`.
    #[test]
    fn translate_error_v2_reads_top_level_message() {
        let mut err = false;
        let line = r#"{"type":"error","error":{"type":"provider.no-route","message":"Model unavailable: opencode/claude-haiku-4-5"}}"#;
        let evs = translate(line, &mut err);
        assert_eq!(types(&evs), vec!["error"]);
        assert_eq!(
            evs[0]["payload"]["text"],
            "Model unavailable: opencode/claude-haiku-4-5"
        );
        assert!(err);
    }

    #[test]
    fn translate_error_marks_error() {
        let mut err = false;
        let line = r#"{"type":"error","error":{"name":"APIError","data":{"message":"boom"}}}"#;
        let evs = translate(line, &mut err);
        assert_eq!(types(&evs), vec!["error"]);
        assert!(err);
    }

    #[test]
    fn translate_step_finish_emits_token_metric() {
        let mut err = false;
        let line = r#"{"type":"step_finish","part":{"type":"step-finish","reason":"stop","tokens":{"total":33739,"input":33609,"output":2,"reasoning":0,"cache":{"write":0,"read":128}},"cost":0.0123}}"#;
        let evs = translate(line, &mut err);
        assert_eq!(types(&evs), vec!["progress"]);
        assert_eq!(evs[0]["payload"]["tokens"], 33739);
        assert_eq!(evs[0]["payload"]["cost_cents"], 1);
        assert!(!err);
    }

    // Real-adapter smoke test: requires the `opencode` CLI and a configured
    // model. Ignored by default; run manually or in nightly CI (spec: real-agent
    // tests need keys, so they are `#[ignore]`).    #[test]
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
                .any(|e| e.get("type").and_then(|t| t.as_str()) == Some("log")
                    || e.get("type").and_then(|t| t.as_str()) == Some("error")),
            "opencode stream should yield log or an error event"
        );
    }
}
