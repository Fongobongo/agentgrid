//! Plan 1.7 (#14): token-budget compression for logs/tool-output before they
//! land in an LLM prompt. Two passes:
//!
//! - [`dedup_lines`]: collapses runs of identical consecutive lines (the
//!   common log-line / stack-trace repeat that dominates noisy tool output)
//!   into a single line + `…×(N-1)` marker — O(n), stable order.
//! - [`smart_truncate`]: hard byte cap, splitting on the last newline before
//!   the cap so we never cut a line in half, with an `[…truncated N bytes]`
//!   trailer.
//!
//! [`compress`] ties the two together and returns the saved-byte metric so a
//! task summary can record `tokens_saved_bytes`.

/// Collapse runs of identical consecutive lines into a single line plus a
/// `…×(N-1)` marker naming how many repeats were folded. Order is preserved;
/// empty lines collapse the same way.
pub fn dedup_lines(input: &str) -> String {
    let lines: Vec<&str> = input.split('\n').collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < lines.len() {
        let cur = lines[i];
        let mut run = 1;
        while i + run < lines.len() && lines[i + run] == cur {
            run += 1;
        }
        out.push_str(cur);
        if run > 2 {
            out.push_str(&format!("\n…×{}", run - 1));
        } else {
            for _ in 1..run {
                out.push('\n');
                out.push_str(cur);
            }
        }
        if i + run < lines.len() {
            out.push('\n');
        }
        i += run;
    }
    out
}

/// Hard byte cap that never splits a line in half: returns up to `max_bytes`,
/// ending at the last newline boundary at or before `max_bytes`, with an
/// `[…truncated N bytes]` trailer when bytes were dropped.
pub fn smart_truncate(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let bytes = input.as_bytes();
    // Walk back to the last newline boundary at or before the cap so we
    // never break a line in half. `cut` lands just past a newline (the newline
    // stays in the kept prefix).
    let mut cut = max_bytes.min(bytes.len());
    while cut > 0 && bytes.get(cut - 1) != Some(&b'\n') {
        cut -= 1;
    }
    // If no newline was found, cut on a char boundary instead.
    let cut = if cut == 0 {
        let mut b = max_bytes;
        while b > 0 && !input.is_char_boundary(b) {
            b -= 1;
        }
        b
    } else {
        cut
    };
    let kept = &input[..cut];
    let dropped = input.len() - cut;
    format!("{kept}\n[…truncated {dropped} bytes]")
}

/// Result of [`compress`] — the compressed text length and the bytes saved
/// vs the input, so a task summary can record `tokens_saved_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compressed {
    pub text_len: usize,
    pub saved_bytes: usize,
}

/// Dedup consecutive identical lines then hard-truncate to `max_bytes`.
/// Returns the compressed text plus how many bytes were saved.
pub fn compress(input: &str, max_bytes: usize) -> (String, Compressed) {
    let deduped = dedup_lines(input);
    let truncated = smart_truncate(&deduped, max_bytes);
    let text_len = truncated.len();
    let saved = input.len().saturating_sub(text_len);
    (
        truncated,
        Compressed {
            text_len,
            saved_bytes: saved,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_collapses_a_run_to_marker() {
        assert_eq!(
            dedup_lines("building\nbuilding\nbuilding\ndone"),
            "building\n…×2\ndone"
        );
    }

    #[test]
    fn dedup_preserves_single_duplicate_as_two_lines() {
        // A run of exactly 2 keeps both lines — the marker would be no shorter
        // than the line it replaces, so no win.
        assert_eq!(dedup_lines("a\na"), "a\na");
        // A run of 3 collapses to one + marker.
        assert_eq!(dedup_lines("a\na\na"), "a\n…×2");
    }

    #[test]
    fn dedup_empty() {
        assert_eq!(dedup_lines(""), "");
        assert_eq!(dedup_lines("x"), "x");
        // Trailing newline round-trips.
        assert_eq!(dedup_lines("a\nb\n"), "a\nb\n");
    }

    #[test]
    fn smart_truncate_keeps_under_cap_without_splitting_lines() {
        let log = "line one\nline two\nline three\n";
        // Cap just past line two's newline (~18 bytes): must end at a newline,
        // not mid-line-three.
        let out = smart_truncate(log, 18);
        assert!(out.starts_with("line one\nline two\n"));
        assert!(out.contains("[…truncated"));
    }

    #[test]
    fn smart_truncate_noop_under_cap() {
        assert_eq!(smart_truncate("short\n", 100), "short\n");
    }

    #[test]
    fn compress_10k_dup_log_shrinks_under_20pct_and_reports_saved() {
        // 10_000 identical lines — the noisiest tool-output shape.
        let log: String = "at com.example.Foo.bar(Foo.java:42)\n".repeat(10_000);
        let (compressed, stats) = compress(&log, 32_000);
        // < 20% of the input.
        assert!(
            stats.text_len < log.len() / 5,
            "{} bytes vs {} input — not under 20%",
            stats.text_len,
            log.len()
        );
        assert!(stats.saved_bytes > 0);
        assert!(compressed.contains("…×"));
    }
}
