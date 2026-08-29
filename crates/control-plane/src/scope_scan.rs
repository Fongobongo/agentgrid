//! Competitor-gap feature (scope-creep guard): deterministic scan of a
//! finished attempt's events for hash/checksum computation the prompt never
//! asked for, inspired by lennney/stop-that-shit ("intercept AI coding
//! agents: stop unrequested hashes, checksums and task-scope creep"). An
//! agent that runs `md5sum`/`sha256sum`/`openssl dgst` over files — or a
//! whole tree — without being told to is padding the session with busywork
//! a diff review can't see. Findings are audit log events (searchable via
//! `/v1/search/events`) and never change the outcome.

use agentgrid_common::EventType;
use serde::Serialize;

/// One finding over an attempt's events. `kind` is a stable machine id,
/// `detail` the human-readable explanation (truncated).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScopeFinding {
    pub kind: &'static str,
    pub detail: String,
}

/// Distinctive hash/checksum commands. Deliberately no bare `sum` (matches
/// "assume"/"consume") and no short `sha`/`md5` fragments (variable names).
const HASH_CMDS: &[&str] = &[
    "md5sum",
    "sha1sum",
    "sha224sum",
    "sha256sum",
    "sha384sum",
    "sha512sum",
    "shasum",
    "b2sum",
    "cksum",
    "openssl dgst",
    "gpg --print-md",
];

/// Markers that turn a hash run into whole-tree/recursive work (louder).
const MASS_MARKERS: &[&str] = &["find ", "xargs", "git ls-files"];

/// Words in the prompt that mean hashing was actually requested. Conservative
/// on purpose: if the prompt so much as mentions hashing, we stay silent
/// rather than risk a false positive on unrelated files.
const PROMPT_HASH_WORDS: &[&str] = &[
    "hash",
    "checksum",
    "sha256",
    "sha512",
    "sha384",
    "sha1",
    "md5",
    "shasum",
    "digest",
    "fingerprint",
];

/// Max detail length per finding (keeps the event small).
const MAX_DETAIL: usize = 200;

/// Scan an attempt's events for unrequested hash/checksum busywork. Pure —
/// unit-tested without a store or network. Returns findings in stable order;
/// an empty vec means the attempt is clean (or the prompt itself asked for
/// hashing).
pub fn scan_events(prompt: &str, events: &[(EventType, String)]) -> Vec<ScopeFinding> {
    let prompt_l = prompt.to_lowercase();
    if PROMPT_HASH_WORDS.iter().any(|w| prompt_l.contains(w)) {
        return Vec::new();
    }
    let mut plain: Vec<String> = Vec::new();
    let mut mass: Vec<String> = Vec::new();
    for (ty, payload) in events {
        if !matches!(
            ty,
            EventType::Tool | EventType::Stdout | EventType::Stderr | EventType::Error
        ) {
            continue;
        }
        for line in payload_lines(payload) {
            let l = line.to_lowercase();
            if !HASH_CMDS.iter().any(|c| l.contains(c)) {
                continue;
            }
            if MASS_MARKERS.iter().any(|m| l.contains(m)) {
                mass.push(line);
            } else {
                plain.push(line);
            }
        }
    }
    let mut out = Vec::new();
    if !plain.is_empty() {
        out.push(ScopeFinding {
            kind: "unrequested_hash",
            detail: summarize(&plain),
        });
    }
    if !mass.is_empty() {
        out.push(ScopeFinding {
            kind: "mass_hash",
            detail: summarize(&mass),
        });
    }
    out
}

/// Sample line + occurrence count.
fn summarize(lines: &[String]) -> String {
    let sample = truncate(&lines[0]);
    if lines.len() > 1 {
        format!("{} hash commands, e.g. `{sample}`", lines.len())
    } else {
        format!("`{sample}`")
    }
}

/// Extract all string values from a stored event payload (JSON), so both log
/// payloads (`{"text": ...}`) and tool_call payloads
/// (`{"name": ..., "input": {...}}`) yield their command text.
fn payload_lines(payload: &str) -> Vec<String> {
    let v: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return vec![payload.to_string()],
    };
    let mut buf = String::new();
    collect_strings(&v, &mut buf);
    if buf.is_empty() {
        vec![payload.to_string()]
    } else {
        buf.lines().map(|s| s.to_string()).collect()
    }
}

fn collect_strings(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        serde_json::Value::Object(m) => {
            for val in m.values() {
                collect_strings(val, out);
            }
        }
        serde_json::Value::Array(a) => {
            for val in a {
                collect_strings(val, out);
            }
        }
        _ => {}
    }
}

fn truncate(s: &str) -> String {
    if s.len() <= MAX_DETAIL {
        s.to_string()
    } else {
        format!("{}…", &s[..MAX_DETAIL - 1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(tool: bool, payload: &str) -> (EventType, String) {
        let ty = if tool {
            EventType::Tool
        } else {
            EventType::Stdout
        };
        (ty, payload.to_string())
    }

    #[test]
    fn clean_attempt_has_no_findings() {
        let events = vec![
            ev(true, r#"{"name":"Bash","input":{"command":"cargo build"}}"#),
            ev(false, r#"{"text":"Building... done"}"#),
        ];
        assert_eq!(scan_events("fix the tests", &events), vec![]);
    }

    #[test]
    fn unrequested_single_hash_flagged() {
        let events = vec![ev(
            true,
            r#"{"name":"Bash","input":{"command":"md5sum src/main.rs"}}"#,
        )];
        let f = scan_events("fix the tests", &events);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, "unrequested_hash");
        assert!(f[0].detail.contains("md5sum"));
    }

    #[test]
    fn requested_hash_is_silent() {
        let events = vec![ev(
            true,
            r#"{"name":"Bash","input":{"command":"sha256sum dist/agentgrid.tar.gz"}}"#,
        )];
        assert_eq!(
            scan_events(
                "build and print the sha256 checksum of the artifact",
                &events
            ),
            vec![]
        );
    }

    #[test]
    fn mass_hash_flagged_separately() {
        let events = vec![ev(
            true,
            r#"{"name":"Bash","input":{"command":"find . -type f -exec sha256sum {} \\;"}}"#,
        )];
        let f = scan_events("add the feature", &events);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, "mass_hash");
    }

    #[test]
    fn plain_and_mass_both_reported() {
        let events = vec![
            ev(true, r#"{"name":"Bash","input":{"command":"cksum x.txt"}}"#),
            ev(
                true,
                r#"{"name":"Bash","input":{"command":"sha256sum a.txt"}}"#,
            ),
            ev(
                true,
                r#"{"name":"Bash","input":{"command":"find / -name '*.log' | xargs md5sum"}}"#,
            ),
        ];
        let f = scan_events("write the doc", &events);
        let kinds: Vec<&str> = f.iter().map(|x| x.kind).collect();
        assert_eq!(kinds, vec!["unrequested_hash", "mass_hash"]);
        assert!(f[0].detail.contains("2 hash commands"));
    }

    #[test]
    fn log_payload_text_scanned_too() {
        let events = vec![ev(false, r#"{"text":"$ shasum -a 256 package.json"}"#)];
        let f = scan_events("deploy it", &events);
        assert_eq!(f.len(), 1);
        assert!(f[0].detail.contains("shasum"));
    }

    #[test]
    fn bare_sum_word_not_flagged() {
        let events = vec![ev(
            true,
            r#"{"name":"Bash","input":{"command":"git commit -m 'summarize the change'"}}"#,
        )];
        assert_eq!(scan_events("commit the work", &events), vec![]);
    }
}
