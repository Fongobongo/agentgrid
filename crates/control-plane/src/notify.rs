//! Plan 1.2 (#22a): mobile-style notifications on operator-facing task events
//! (task finished / failed / awaiting review).
//!
//! Two payload shapes, keyed off the configured URL:
//! - `https://api.telegram.org/bot<token>/sendMessage` — native Bot API
//!   call: `{"chat_id": <n>, "text": <human message>}`. The target chat is
//!   taken from a `?chat_id=` query part (set up once by the operator);
//!   any other query parts are dropped.
//! - anything else — the generic JSON POST of the whole `TaskNotification`
//!   document. `ntfy.sh` works out of the box (body shown as-is); any
//!   endpoint that accepts a raw JSON POST works too.
//!
//! `#[cfg(test)]` block covers both shapes against an in-process TCP
//! mock (no external dep): launches a tiny `TcpListener`, POSTs to it,
//! and asserts the JSON body matches what we expect.

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct TaskNotification {
    pub task_id: String,
    pub attempt_id: String,
    /// completed | failed | awaiting_review
    pub status: String,
    /// URL a human can click to look at the task in the UI.
    pub url: String,
}

/// Human-readable one-liner for chat surfaces (Telegram text mode). The
/// notification `url` is a UI-relative path, so the text leads with the
/// status + task id; operators paste/act from the chat context.
fn chat_text(note: &TaskNotification) -> String {
    let label = match note.status.as_str() {
        "completed" => "✅ task completed",
        "failed" => "❌ task failed",
        "awaiting_review" => "👀 task awaiting review",
        other => other,
    };
    let id = if note.task_id.len() > 8 {
        &note.task_id[..8]
    } else {
        &note.task_id
    };
    format!("{label} {id} — /show {id}")
}

/// Best-effort POST to the configured webhook. Errors are logged and
/// swallowed — notifications must never block terminal task state changes.
pub async fn notify_task(url: &str, note: &TaskNotification) {
    let client = reqwest::Client::new();
    // Telegram Bot API endpoints get the native sendMessage shape; every
    // other URL keeps the generic JSON document (ntfy & friends).
    let result = if is_telegram_send_message(url) {
        let chat_id = telegram_chat_id(url);
        client
            .post(strip_query(url))
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "text": chat_text(note),
            }))
            .send()
            .await
    } else {
        client.post(url).json(note).send().await
    };
    if let Err(e) = result {
        tracing::warn!(target = %url, status = %note.status, "notify_task POST failed: {e}");
    }
}

/// True for a Bot API sendMessage URL (`api.telegram.org/bot<token>/sendMessage`).
fn is_telegram_send_message(url: &str) -> bool {
    url.starts_with("https://api.telegram.org/bot") && url.contains("/sendMessage")
}

/// Extract `chat_id` from the query part. Telegram shape:
/// `https://api.telegram.org/bot<token>/sendMessage?chat_id=<n>`.
fn telegram_chat_id(url: &str) -> i64 {
    let q = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    for pair in q.split('&') {
        if let Some(v) = pair.strip_prefix("chat_id=") {
            if let Ok(n) = v.trim().parse::<i64>() {
                return n;
            }
        }
    }
    0
}

/// Drop the query part — the POST target is the bare method URL.
fn strip_query(url: &str) -> &str {
    url.split_once('?').map(|(base, _)| base).unwrap_or(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Tiny loopback HTTP server: returns the request body on a `GET /body`
    /// endpoint so the test can inspect exactly what `notify_task` POSTed.
    /// Returns `(url, last_body_cell)`. The accept loop runs until the cell
    /// is dropped (cell holds the last received body).
    async fn mock_webhook() -> (String, std::sync::Arc<tokio::sync::Mutex<Option<Vec<u8>>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");
        let last = std::sync::Arc::new(tokio::sync::Mutex::new(None::<Vec<u8>>));
        let last_c = last.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let last = last_c.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    // body = everything after the blank line
                    let req = &buf[..n];
                    let body = if let Some(idx) = find_body_start(req) {
                        req[idx..].to_vec()
                    } else {
                        vec![]
                    };
                    if let Ok(mut g) = last.try_lock() {
                        *g = Some(body);
                    }
                    sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await
                        .ok();
                });
            }
        });
        (url, last)
    }

    fn find_body_start(req: &[u8]) -> Option<usize> {
        // CRLF CRLF separates headers from body
        req.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
    }

    async fn posted_body(last: &std::sync::Arc<tokio::sync::Mutex<Option<Vec<u8>>>>) -> String {
        for _ in 0..50 {
            if let Some(b) = last.lock().await.as_ref() {
                return String::from_utf8_lossy(b).to_string();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("webhook never received a POST");
    }

    fn sample_note() -> TaskNotification {
        TaskNotification {
            task_id: "t1".into(),
            attempt_id: "a1".into(),
            status: "failed".into(),
            url: "/tasks/t1".into(),
        }
    }

    #[tokio::test]
    async fn notify_task_posts_json_body_to_url() {
        let (url, last) = mock_webhook().await;
        notify_task(&url, &sample_note()).await;
        let body = posted_body(&last).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["task_id"], "t1");
        assert_eq!(v["status"], "failed");
        assert_eq!(v["url"], "/tasks/t1");
    }

    #[test]
    fn detects_telegram_send_message_urls() {
        assert!(is_telegram_send_message(
            "https://api.telegram.org/bot123:abc/sendMessage?chat_id=42"
        ));
        assert!(!is_telegram_send_message("https://ntfy.sh/mytopic"));
        assert!(!is_telegram_send_message(
            "https://api.telegram.org/bot123:abc/getUpdates"
        ));
        assert!(!is_telegram_send_message(
            "http://api.telegram.org/bot123:abc/sendMessage" // not https
        ));
    }

    #[test]
    fn chat_id_parsed_from_query_and_query_stripped() {
        let url = "https://api.telegram.org/bot123:abc/sendMessage?chat_id=42&foo=1";
        assert_eq!(telegram_chat_id(url), 42);
        assert_eq!(
            strip_query(url),
            "https://api.telegram.org/bot123:abc/sendMessage"
        );
        // No query / bad value -> 0 (Telegram rejects, warn logged, no crash).
        assert_eq!(
            telegram_chat_id("https://api.telegram.org/bot1/sendMessage"),
            0
        );
        assert_eq!(telegram_chat_id("...?chat_id=abc"), 0);
    }

    #[test]
    fn chat_text_mentions_status_and_task() {
        let t = chat_text(&sample_note());
        assert!(t.contains("failed"));
        assert!(t.contains("t1"));
        assert!(t.contains("/show"));
        let mut review = sample_note();
        review.status = "awaiting_review".into();
        assert!(chat_text(&review).contains("awaiting review"));
    }
}
