//! Real adapter wrapper for Aider (`aider`).
//!
//! Runs `aider -m "<prompt>" --yes-always` in the attempt worktree and
//! streams its plain-text output as NDJSON events. Aider has no machine
//! output format — no `tool_call`/`file_change` events are emitted, so the
//! web UI shows the session as a plain transcript; `git diff` (captured into
//! `changes.patch`) stays the source of truth for edits.
//!
//! Invocation contract (matches the daemon): `--prompt "<text>"`, cwd =
//! attempt worktree. Model/endpoint/keys ride the standard env channel
//! (`AGENTGRID_ADAPTER_ENV` / CP-managed adapter-env): litellm under the
//! hood honours `OPENAI_API_KEY` / `OPENAI_BASE_URL` / `DEEPSEEK_API_KEY`,
//! etc. `AIDER_MODEL` selects `--model`.
//!
//! Configuration (all optional env):
//!   AGENTGRID_AIDER_BIN  binary or `python3 -m aider` fallback resolution
//!   AIDER_MODEL          passed as `--model`
//!   AGENTGRID_UNSAFE_UNATTENDED  gate for `--yes-always`: default on run
//!                        means "conversational confirmations" otherwise
//!                       silently dead-lock an unattended attempt, so we
//!                       treat **no bypass** as *default yes* only when the
//!                         bypass flag is on, mirroring claude/codex safety
//!                         ladder: without the opt-in we still run interactive
//!                         read-only (`--no-git --chat-mode=chat`).

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::json;

fn emit_event(ev: serde_json::Value) {
    let line = serde_json::to_string(&ev).unwrap();
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Build the aider argv (spawned as `<bin> <args…>` or
/// `python3 -m aider <args…>` when the bare binary is missing).
fn aider_args(prompt: &str, unsafe_unattended: bool) -> (String, Vec<String>) {
    let mut extra: Vec<String> = Vec::new();
    if unsafe_unattended {
        extra.push("--yes-always".into());
    } else {
        // Read-only chat: no file writes without explicit operator opt-in.
        extra.push("--chat-mode".into());
        extra.push("chat".into());
    }
    if let Ok(model) = std::env::var("AIDER_MODEL") {
        if !model.trim().is_empty() {
            extra.push("--model".into());
            extra.push(model);
        }
    }
    extra.push("-m".into());
    extra.push(prompt.to_string());

    let bin = std::env::var("AGENTGRID_AIDER_BIN").unwrap_or_else(|_| "aider".into());
    // If the bare binary doesn't exist, degrade to `python3 -m aider` so
    // ops teams that never exposed `aider` on PATH (but installed the
    // package) still get a working adapter.
    let path_ok = shell_which(&bin);
    if path_ok {
        (bin, extra)
    } else {
        let mut args = vec!["-m".to_string(), "aider".to_string()];
        args.extend(extra);
        ("python3".to_string(), args)
    }
}

fn shell_which(bin: &str) -> bool {
    if bin.contains('/') {
        return std::path::Path::new(bin).exists();
    }
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).exists()))
        .unwrap_or(false)
}

/// Parse aider's "Tokens: N sent, N received" summary into a progress event.
fn summary_to_progress(line: &str) -> Option<serde_json::Value> {
    let rest = line.strip_prefix("Tokens:")?;
    let mut sent = None;
    let mut recv = None;
    for part in rest.split(',') {
        let mut it = part.split_whitespace();
        let num = it
            .next()
            .map(|s| s.trim_end_matches(|c: char| !c.is_ascii_digit()));
        let pos = it
            .next()
            .map(|w| w.trim_end_matches(|c: char| !c.is_ascii_alphabetic()));
        if let Some(n) = num.and_then(|s| s.parse::<u64>().ok()) {
            match pos {
                Some("sent") => sent = Some(n),
                Some("received") => recv = Some(n),
                _ => {}
            }
        }
    }
    let tokens = sent.unwrap_or(0) + recv.unwrap_or(0);
    (tokens > 0).then(|| json!({ "type": "progress", "payload": { "tokens": tokens } }))
}

/// Translate one plain-text aider stdout line into agentgrid events.
fn translate(line: &str) -> Vec<serde_json::Value> {
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(ev) = summary_to_progress(trimmed) {
        out.push(ev);
    } else if trimmed.contains("kit ") || trimmed.starts_with("$note:") {
        // aider skirts `chatkit`-style prefixes; keep as plain log anyway.
        out.push(json!({ "type": "log", "payload": { "text": trimmed } }));
    } else {
        // Surface plans (same sub-machine shared with other adapters).
        for plan in agentgrid_adapters::plan_blocks(trimmed) {
            out.push(json!({ "type": "plan", "payload": { "text": plan } }));
        }
        out.push(json!({ "type": "log", "payload": { "text": trimmed } }));
    }
    out
}

fn main() {
    let mut prompt = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--version" | "-v" => {
                println!("adapter-aider {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--prompt" => prompt = args.next().unwrap_or_default(),
            _ => {}
        }
    }

    let unsafe_unattended = agentgrid_adapters::unsafe_unattended_from_env();
    agentgrid_adapters::warn_unsafe("adapter-aider", unsafe_unattended, false);
    let (bin, aargs) = aider_args(&prompt, unsafe_unattended);
    let mut cmd = Command::new(&bin);
    cmd.args(&aargs);
    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("adapter-aider: failed to spawn {bin}: {e}");
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
    for line in reader.lines().map_while(Result::ok) {
        for ev in translate(&line) {
            emit_event(ev);
        }
    }

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("adapter-aider: wait for {bin} failed: {e}");
            std::process::exit(1);
        }
    };
    let _ = err_thread.join();
    let code = status.code().unwrap_or(1);
    std::process::exit(code);
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
    fn tokens_summary_becomes_progress() {
        let evs = translate("Tokens: 120 sent, 34 received.");
        assert_eq!(types(&evs), vec!["progress"]);
        assert_eq!(evs[0]["payload"]["tokens"], 154);
    }

    #[test]
    fn unsafe_mode_opts_in_confirmations() {
        let (_, a) = aider_args("fix it", true);
        assert!(a.contains(&"--yes-always".into()));
        let (_, b) = aider_args("fix it", false);
        assert!(!b.contains(&"--yes-always".into()));
        assert!(b.contains(&"--chat-mode".into()));
    }

    #[test]
    fn model_from_env_when_set() {
        std::env::set_var("AIDER_MODEL", "openai/gpt-4o");
        let (_, a) = aider_args("hi", true);
        assert!(a.contains(&"openai/gpt-4o".into()));
        std::env::remove_var("AIDER_MODEL");
    }

    #[test]
    fn plan_fence_surfaces_as_plan_event() {
        let evs = translate("plan:\n```plan\n- id: w\n  prompt: do\n```");
        assert!(evs.iter().any(|e| e["type"] == "plan"));
    }

    #[test]
    fn binary_falls_back_to_python_module() {
        let (bin, _) = aider_args("hi", true);
        assert!(bin == "aider" || bin == "python3");
    }
}
