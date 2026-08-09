//! Plan 1.2 (#22a): mobile-style notifications on operator-facing task events
//! (task finished / failed / awaiting review). Implementation is deliberately
//! minimal: a single async fire-and-forget POST to a configured URL.
//!
//! `ntfy.sh` works out of the box (POST body is shown as-is).
//! Telegram/FCM-style endpoints also work if they accept a raw JSON POST.
//!
//! `#[cfg(test)]` block covers the happy path against an in-process TCP
//! mock (no external dep): launches a tiny `TcpListener`, POSTs to it,
//! and asserts the JSON body matches the `status` we expect.

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

/// Best-effort POST to the configured webhook. Errors are logged and
/// swallowed — notifications must never block terminal task state changes.
pub async fn notify_task(url: &str, note: &TaskNotification) {
    let client = reqwest::Client::new();
    if let Err(e) = client.post(url).json(note).send().await {
        tracing::warn!(target = %url, status = %note.status, "notify_task POST failed: {e}");
    }
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

    #[tokio::test]
    async fn notify_task_posts_json_body_to_url() {
        let (url, last) = mock_webhook().await;
        let note = TaskNotification {
            task_id: "t1".into(),
            attempt_id: "a1".into(),
            status: "failed".into(),
            url: "/tasks/t1".into(),
        };
        notify_task(&url, &note).await;
        let body = posted_body(&last).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["task_id"], "t1");
        assert_eq!(v["status"], "failed");
        assert_eq!(v["url"], "/tasks/t1");
    }
}
