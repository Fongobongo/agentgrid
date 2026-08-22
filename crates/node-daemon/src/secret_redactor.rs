//! Streaming secret redactor with chunk overlap.
//! Masks secrets that may be split across chunk/line boundaries.

/// A streaming redactor that masks secrets in a byte stream.
/// Uses line-buffered reading with overlap to catch secrets split across
/// chunk boundaries. Also enforces a maximum line length (line_cap).
pub struct StreamingRedactor {
    secrets: Vec<String>,
    /// Minimum secret length (skip shorter ones).
    min_len: usize,
    /// Maximum line length before forced flush (truncation).
    line_cap: usize,
    /// Internal buffer for accumulated bytes not yet emitted.
    buf: Vec<u8>,
}

impl StreamingRedactor {
    pub fn new(secrets: Vec<String>, min_len: usize, line_cap: usize) -> Self {
        Self {
            secrets,
            min_len,
            line_cap,
            buf: Vec::new(),
        }
    }

    /// Feed a chunk of data, returns a vector of complete masked lines (without newline).
    /// If accumulated data exceeds line_cap without a newline, emits a truncated line.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(chunk);

        let mut lines = Vec::new();

        loop {
            // Find the next newline
            if let Some(nl_pos) = self.buf.iter().position(|&b| b == b'\n') {
                // Check line cap on the unmasked line
                if nl_pos >= self.line_cap {
                    // Audit X-N3: forced split of an over-long NEWLINE-
                    // terminated line. This branch used to drop everything
                    // past the cap without carrying an overlap, so a secret
                    // straddling the cap leaked as an unmasked fragment.
                    // Emit the capped window, then keep the same trailing
                    // overlap the no-newline branch uses so the straddling
                    // secret is masked whole in the next emitted piece.
                    let truncated = &self.buf[..self.line_cap];
                    let mut masked = mask_line(truncated, &self.secrets, self.min_len);
                    masked.push_str("... [truncated]");
                    lines.push(masked.into_bytes());
                    let overlap = self.overlap_len();
                    self.buf.drain(..self.line_cap - overlap);
                    continue;
                }

                // Normal line - mask and emit
                let masked = mask_line(&self.buf[..nl_pos], &self.secrets, self.min_len);
                lines.push(masked.into_bytes());

                // Remove the processed line + newline from buffer
                self.buf = self.buf[nl_pos + 1..].to_vec();
            } else {
                // No complete line in buffer
                // If buffer exceeds line cap, flush truncated portion and keep draining
                if self.buf.len() >= self.line_cap {
                    let truncated = &self.buf[..self.line_cap];
                    let mut masked = mask_line(truncated, &self.secrets, self.min_len);
                    masked.push_str("... [truncated]");
                    lines.push(masked.into_bytes());
                    // Keep draining: remove the flushed portion from buffer,
                    // but carry a trailing overlap (>= max secret length) into
                    // the next chunk so a secret spanning this forced boundary
                    // is masked in full there instead of leaking as two
                    // unmasked fragments.
                    let overlap = self.overlap_len();
                    self.buf = self.buf[self.line_cap - overlap..].to_vec();
                    continue;
                }
                break;
            }
        }

        lines
    }

    /// Trailing bytes re-processed after a forced line_cap split: the longest
    /// secret, capped below `line_cap` so each split still drains the buffer.
    fn overlap_len(&self) -> usize {
        let max_secret = self.secrets.iter().map(|s| s.len()).max().unwrap_or(0);
        max_secret.min(self.line_cap.saturating_sub(1))
    }

    /// Finish processing, returns any remaining partial line as a masked line.
    pub fn finish(self) -> Option<Vec<u8>> {
        if !self.buf.is_empty() {
            if self.buf.len() >= self.line_cap {
                let truncated = &self.buf[..self.line_cap];
                let mut masked = mask_line(truncated, &self.secrets, self.min_len);
                masked.push_str("... [truncated]");
                Some(masked.into_bytes())
            } else {
                let masked = mask_line(&self.buf, &self.secrets, self.min_len);
                Some(masked.into_bytes())
            }
        } else {
            None
        }
    }
}

/// Mask secrets in a single line.
/// Also masks base64 and percent-encoded variants.
fn mask_line(line: &[u8], secrets: &[String], min_len: usize) -> String {
    let s = String::from_utf8_lossy(line).to_string();
    mask_secrets(&s, secrets, min_len)
}

/// Mask secrets in a string, with minimum length filter.
/// Also masks base64 and percent-encoded variants.
fn mask_secrets(line: &str, secrets: &[String], min_len: usize) -> String {
    let mut s = line.to_string();
    for sec in secrets {
        if sec.len() >= min_len {
            s = s.replace(sec, "***");
            s = s.replace(&base64_encode(sec.as_bytes()), "***");
            s = s.replace(&url_encode(sec), "***");
        }
    }
    s
}

/// Minimal base64 encoder (no external dep) for secret-variant masking.
fn base64_encode(bytes: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).map(|&b| b as u32).unwrap_or(0);
        let b2 = chunk.get(2).map(|&b| b as u32).unwrap_or(0);
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TBL[(n >> 18) as usize & 63] as char);
        out.push(TBL[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(TBL[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TBL[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Minimal percent-encoder (non-alphanumeric -> %XX) for secret-variant masking.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_secret_in_single_chunk() {
        let mut redactor = StreamingRedactor::new(vec!["secret123".to_string()], 6, 1024 * 1024);
        let lines = redactor.feed(b"token=secret123\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(String::from_utf8_lossy(&lines[0]), "token=***");
    }

    #[test]
    fn masks_secret_split_across_chunks() {
        let mut redactor = StreamingRedactor::new(vec!["secret123".to_string()], 6, 1024 * 1024);
        // First chunk ends mid-secret (no newline yet)
        let lines1 = redactor.feed(b"token=sec");
        assert!(lines1.is_empty()); // No complete line yet
                                    // Second chunk completes the secret and line
        let lines2 = redactor.feed(b"ret123\n");
        assert_eq!(lines2.len(), 1);
        assert_eq!(String::from_utf8_lossy(&lines2[0]), "token=***");
    }

    #[test]
    fn multiple_secrets_in_line() {
        let mut redactor = StreamingRedactor::new(
            vec!["secret123".to_string(), "api-key-456".to_string()],
            6,
            1024 * 1024,
        );
        let lines = redactor.feed(b"token=secret123 key=api-key-456\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(String::from_utf8_lossy(&lines[0]), "token=*** key=***");
    }

    #[test]
    fn masks_base64_variant() {
        let mut redactor = StreamingRedactor::new(vec!["secret123".to_string()], 6, 1024 * 1024);
        let b64 = base64_encode(b"secret123");
        let lines = redactor.feed(format!("token={b64}\n").as_bytes());
        assert_eq!(lines.len(), 1);
        assert_eq!(String::from_utf8_lossy(&lines[0]), "token=***");
    }

    #[test]
    fn masks_url_encoded_variant() {
        let mut redactor = StreamingRedactor::new(vec!["secret@123".to_string()], 6, 1024 * 1024);
        let encoded = url_encode("secret@123");
        let lines = redactor.feed(format!("token={encoded}\n").as_bytes());
        assert_eq!(lines.len(), 1);
        assert_eq!(String::from_utf8_lossy(&lines[0]), "token=***");
    }

    #[test]
    fn respects_min_len() {
        let mut redactor = StreamingRedactor::new(vec!["abc".to_string()], 6, 1024 * 1024);
        let lines = redactor.feed(b"token=abc\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(String::from_utf8_lossy(&lines[0]), "token=abc"); // Not masked, too short
    }

    #[test]
    fn handles_empty_chunk() {
        let mut redactor = StreamingRedactor::new(vec!["secret123".to_string()], 6, 1024 * 1024);
        let lines = redactor.feed(b"");
        assert!(lines.is_empty());
    }

    #[test]
    fn handles_no_secrets() {
        let mut redactor = StreamingRedactor::new(vec![], 6, 1024 * 1024);
        let lines = redactor.feed(b"hello world\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(String::from_utf8_lossy(&lines[0]), "hello world");
    }

    #[test]
    fn finish_emits_partial_line() {
        let mut redactor = StreamingRedactor::new(vec!["secret123".to_string()], 6, 1024 * 1024);
        redactor.feed(b"token=secret123"); // No newline
        let final_line = redactor.finish();
        assert!(final_line.is_some());
        assert_eq!(
            String::from_utf8_lossy(final_line.unwrap().as_ref()),
            "token=***"
        );
    }

    #[test]
    fn line_cap_truncates_long_line() {
        // Line cap = 16 bytes
        let mut redactor = StreamingRedactor::new(vec!["secret".to_string()], 6, 16);
        // 30 bytes, no newline
        let input: Vec<u8> = vec![b'x'; 30];
        let lines = redactor.feed(&input);
        // With the trailing overlap (6 = secret length) each forced split
        // advances 10 bytes: 30 bytes -> two truncated lines from feed,
        // remainder stays in buffer.
        assert_eq!(lines.len(), 2, "expected 2 truncated lines from feed");
        assert!(String::from_utf8_lossy(&lines[0]).contains("... [truncated]"));
        assert!(String::from_utf8_lossy(&lines[1]).contains("... [truncated]"));
        // finish() emits the remaining 10 bytes as final line (no truncation marker)
        let final_line = redactor.finish();
        assert!(final_line.is_some());
        let tail = String::from_utf8_lossy(final_line.unwrap().as_ref()).to_string();
        assert!(!tail.contains("... [truncated]"));
        assert_eq!(tail.len(), 10);
    }

    #[test]
    fn masks_secret_spanning_forced_chunk_boundary() {
        // Cap 16; the secret starts before the forced boundary and ends after
        // it. The overlap carries it whole into the next masking window, so
        // the full secret must never appear in the output.
        let mut redactor = StreamingRedactor::new(vec!["secret123".to_string()], 6, 16);
        let input = format!("{}secret123{}", "x".repeat(12), "y".repeat(4));
        let mut lines = redactor.feed(input.as_bytes());
        if let Some(tail) = redactor.finish() {
            lines.push(tail);
        }
        let joined = lines
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !joined.contains("secret123"),
            "secret spanning a forced boundary must be masked: {joined}"
        );
        assert!(joined.contains("***"), "mask marker expected: {joined}");
    }

    #[test]
    fn line_cap_respects_newlines() {
        let mut redactor = StreamingRedactor::new(vec!["secret".to_string()], 6, 16);
        // Two short lines, each under cap
        let lines = redactor.feed(b"hello\nworld\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(String::from_utf8_lossy(&lines[0]), "hello");
        assert_eq!(String::from_utf8_lossy(&lines[1]), "world");
    }

    #[test]
    fn masks_secret_spanning_cap_in_newlined_line() {
        // Audit X-N3 regression: an over-long line WITH a newline must carry
        // the same overlap as the no-newline forced split, so a secret
        // straddling the cap is masked whole instead of leaking a fragment.
        let mut redactor = StreamingRedactor::new(vec!["secret123".to_string()], 6, 16);
        let input = format!("{}secret123{}\n", "x".repeat(12), "y".repeat(4));
        let mut lines = redactor.feed(input.as_bytes());
        if let Some(tail) = redactor.finish() {
            lines.push(tail);
        }
        let joined = lines
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            !joined.contains("secret123"),
            "secret straddling cap in a newline-terminated line must be masked: {joined}"
        );
    }
}
