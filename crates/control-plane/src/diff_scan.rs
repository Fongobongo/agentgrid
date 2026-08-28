//! Competitor-gap feature (diff pattern-scan): deterministic pre-pass over a
//! finished attempt's `changes.patch`, inspired by alibaba/open-code-review's
//! "deterministic pipelines + LLM" split. The secret redactor masks agent LOGS,
//! but the diff artifact itself is never scanned — a committed credential or a
//! 5k-line dump ships straight into the review page. These cheap,
//! hallucination-free rules catch what the redactor cannot.

use serde::Serialize;

/// One deterministic finding over a patch. `rule` is a stable machine id,
/// `file` the affected path (may be empty for patch-level rules), `detail`
/// the human-readable explanation (truncated).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiffFinding {
    pub rule: &'static str,
    pub file: String,
    pub detail: String,
}

/// Max added lines per file before the `oversized_additions` rule fires.
const MAX_ADDED_LINES: usize = 500;
/// Max detail length per finding (keeps the event small).
const MAX_DETAIL: usize = 200;

/// Scan a unified diff for deterministic red flags. Pure — unit-tested
/// without a store or network. Returns findings in file order; an empty vec
/// means the patch is clean.
pub fn scan_patch(patch: &str) -> Vec<DiffFinding> {
    let mut findings = Vec::new();
    let mut current_file = String::new();
    let mut added_lines = 0usize;

    // Secret patterns: conservative prefixes + minimum length so ordinary
    // code (e.g. `sk_learners`) is not flagged. Longer, more specific
    // prefixes first — the loop breaks on the first match per line.
    let secret_rules: &[(&str, &str)] = &[
        ("anthropic_style_key", "sk-ant-"),
        ("openai_style_key", "sk-"),
        ("github_fine_grained", "github_pat_"),
        ("github_pat", "ghp_"),
        ("aws_access_key", "AKIA"),
        ("gcp_service_account", "AKID"),
        ("slack_token", "xoxb-"),
    ];
    let min_len = |prefix: &str| prefix.len() + 16;

    for raw in patch.lines() {
        let line = raw.trim_end_matches('\r');
        // Track the current file across `+++ b/...` headers.
        if let Some(stripped) = line.strip_prefix("+++ ") {
            current_file = stripped
                .trim()
                .trim_start_matches("b/")
                .trim_end_matches('\t')
                .to_string();
        }
        if line.starts_with("Binary files") || line.starts_with("GIT binary patch") {
            findings.push(DiffFinding {
                rule: "binary_blob",
                file: truncate(&current_file),
                detail: "binary content added (review the blob outside the diff)".into(),
            });
            continue;
        }
        if line.starts_with("+") && !line.starts_with("+++") {
            added_lines += 1;
            let content = &line[1..];
            for (rule, prefix) in secret_rules {
                if content.starts_with(*prefix) && content.len() >= min_len(prefix) {
                    findings.push(DiffFinding {
                        rule,
                        file: truncate(&current_file),
                        detail: format!(
                            "possible secret `{}…` added",
                            truncate(&content[..content.len().min(40)])
                        ),
                    });
                    break;
                }
            }
            if content.starts_with("-----BEGIN") && content.contains("PRIVATE KEY-----") {
                findings.push(DiffFinding {
                    rule: "private_key",
                    file: truncate(&current_file),
                    detail: "PEM private key material added".into(),
                });
            }
        }
    }
    if added_lines > MAX_ADDED_LINES {
        findings.push(DiffFinding {
            rule: "oversized_additions",
            file: String::new(),
            detail: format!(
                "patch adds {added_lines} lines (>{MAX_ADDED_LINES}); split the change"
            ),
        });
    }
    findings
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

    #[test]
    fn clean_patch_has_no_findings() {
        let patch = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-old\n+new";
        assert_eq!(scan_patch(patch), vec![]);
    }

    #[test]
    fn secrets_are_flagged_with_file() {
        let patch = "diff --git a/conf b/conf\n--- a/conf\n+++ b/conf\n+sk-ant-api03-abcdefghijklmnopqrstuvwxyz123456\n+AKIAIOSFODNN7EXAMPLE";
        let findings = scan_patch(patch);
        let rules: Vec<&str> = findings.iter().map(|f| f.rule).collect();
        assert!(rules.contains(&"anthropic_style_key"));
        assert!(rules.contains(&"aws_access_key"));
        assert!(findings.iter().all(|f| f.file == "conf"));
    }

    #[test]
    fn private_key_material_flagged() {
        let patch = "+++ b/key.pem\n+-----BEGIN RSA PRIVATE KEY-----\n+MIIEow...\n+-----END RSA PRIVATE KEY-----";
        let findings = scan_patch(patch);
        assert!(findings.iter().any(|f| f.rule == "private_key"));
    }

    #[test]
    fn binary_blob_flagged() {
        let patch = "+++ b/blob.bin\nBinary files /dev/null and b/blob.bin differ";
        let findings = scan_patch(patch);
        assert!(findings.iter().any(|f| f.rule == "binary_blob"));
    }

    #[test]
    fn oversized_additions_flagged_once() {
        let mut patch = String::from("+++ b/big.rs\n");
        for i in 0..600 {
            patch.push_str(&format!("+line {i}\n"));
        }
        let findings = scan_patch(&patch);
        assert_eq!(
            findings
                .iter()
                .filter(|f| f.rule == "oversized_additions")
                .count(),
            1
        );
    }

    #[test]
    fn short_prefix_matches_are_not_secrets() {
        // `sk_learners` is a variable, not a key: too short to match.
        let patch = "+++ b/x.rs\n+let sk_learners = 3;";
        assert_eq!(scan_patch(patch), vec![]);
    }
}
