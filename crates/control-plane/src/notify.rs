//! Plan 1.2 (#22a): mobile-style notifications on operator-facing task events
//! (task finished / failed / awaiting review). Implementation is deliberately
//! minimal: a single async fire-and-forget POST to a configured URL.
//!
//! `ntfy.sh` works out of the box (POST body is shown as-is).
//! Telegram/FCM-style endpoints also work if they accept a raw JSON POST.

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
