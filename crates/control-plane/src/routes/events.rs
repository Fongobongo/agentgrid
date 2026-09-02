//! Event + poll routes: SSE stream, event history, scheduler poll.

use std::sync::Arc;

use agentgrid_common::{EventsQuery, PollRequest, PollResponse, TaskEvent};
use axum::{
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::Sse,
    Json,
};
use futures_core::Stream;

use crate::auth::AuthedNode;
use crate::AppState;

/// Resolve the SSE `after` cursor for a reconnect. `Last-Event-ID` header
/// (browser default) carries the global `ingest_id` of the last delivered
/// event (Hardening P0 item 9), and an explicit `after_ingest` query wins.
/// Legacy `after_sequence` (per-attempt) is still accepted when neither is
/// set, so pre-0037 clients keep working.
fn sse_resume_after(
    after_ingest: Option<u64>,
    after_sequence: u64,
    last_event_id: Option<&axum::http::HeaderValue>,
) -> (Option<u64>, u64) {
    let last = last_event_id
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    match last {
        // `Last-Event-ID` is an ingest_id (the id: field on events since 0037).
        Some(last) => (Some(last.max(after_ingest.unwrap_or(0))), after_sequence),
        None => (after_ingest, after_sequence),
    }
}

pub async fn events_stream(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
    Query(q): Query<EventsQuery>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>> {
    use axum::response::sse::{Event, Sse};
    use std::time::Duration;
    // SSE reconnect resume: `Last-Event-ID` header (browser default on
    // reconnect) carries the global ingest_id of the last delivered event, but
    // an explicit `after_ingest` query wins (lets a client force a different
    // point). This gives no gaps/no dups across attempts: the next poll starts
    // after the last delivered ingest_id.
    let (mut after_ingest, after_sequence) = sse_resume_after(
        q.after_ingest,
        q.after_sequence,
        headers.get("Last-Event-ID"),
    );
    let stream = async_stream::stream! {
        let mut consecutive_errors = 0u32;
        loop {
            match state
                .store
                .get_events(&task_id, after_ingest, after_sequence, Some(500))
                .await
            {
                Ok(events) => {
                    consecutive_errors = 0;
                    for e in events {
                        // Track both cursors: the global ingest_id drives
                        // pagination; the legacy sequence stays accurate for
                        // old clients that keep polling after_sequence.
                        after_ingest = Some(after_ingest.unwrap_or(0).max(e.ingest_id));
                        if let Ok(data) = serde_json::to_string(&e) {
                            // SSE `id:` is the global ingest_id so a browser
                            // sends `Last-Event-ID` as an ingest cursor on
                            // reconnect (Hardening P0 item 9).
                            yield Ok(
                                Event::default()
                                    .event("task-event")
                                    .id(e.ingest_id.to_string())
                                    .data(data),
                            );
                        }
                    }
                }
                Err(e) => {
                    // Don't hang forever on a broken store: surface repeated
                    // failures as an error event and end the stream (clients
                    // reconnect via Last-Event-ID without losing events).
                    consecutive_errors += 1;
                    tracing::warn!("SSE get_events failed ({consecutive_errors}): {e}");
                    if consecutive_errors >= 20 {
                        yield Ok(Event::default()
                            .event("error")
                            .data("event stream unavailable; reconnecting required"));
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

/// UI change stream (plan 3.2): emits `hello` on connect and `change` when
/// the task/node/workflow-run status fingerprint differs from the last one
/// seen. Clients refetch their lists on `change`; idle clients get no
/// traffic. The fingerprint is polled server-side every 500 ms, so a status
/// change reaches the UI in well under a second.
pub async fn changes_stream(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>> {
    use axum::response::sse::{Event, Sse};
    use std::time::Duration;
    let stream = async_stream::stream! {
        let mut last: Option<String> = None;
        let mut consecutive_errors = 0u32;
        loop {
            match state.store.status_fingerprint().await {
                Ok(fp) => {
                    consecutive_errors = 0;
                    if let Ok(data) = serde_json::to_string(&fp) {
                        if last.as_deref() != Some(data.as_str()) {
                            let kind = if last.is_some() { "change" } else { "hello" };
                            last = Some(data.clone());
                            yield Ok(Event::default().event(kind).data(data));
                        }
                    }
                }
                Err(e) => {
                    consecutive_errors += 1;
                    tracing::warn!("changes_stream fingerprint failed ({consecutive_errors}): {e}");
                    if consecutive_errors >= 20 {
                        yield Ok(Event::default()
                            .event("error")
                            .data("change stream unavailable; reconnecting required"));
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

pub async fn get_events(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<Vec<TaskEvent>>, StatusCode> {
    // Hardening P2 item 19: never return an empty list on a DB error — surface
    // storage outage as 503 with a machine-readable code so the client does
    // not read it as "no events".
    match state
        .store
        .get_events(&task_id, q.after_ingest, q.after_sequence, q.limit)
        .await
    {
        Ok(e) => Ok(Json(e)),
        Err(e) => {
            tracing::error!("get_events failed: {e}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// Plan 0.3 stage 0: (poll requests served, cumulative handler ms).
static POLL_REQUESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static POLL_DURATION_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn poll_stats() -> (u64, u64) {
    (
        POLL_REQUESTS.load(std::sync::atomic::Ordering::Relaxed),
        POLL_DURATION_MS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

pub async fn poll(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    headers: axum::http::HeaderMap,
    Json(mut req): Json<PollRequest>,
) -> (StatusCode, Json<PollResponse>) {
    let start = std::time::Instant::now();
    // The authenticated node id is the source of truth; ignore any client-supplied id.
    req.node_id = auth.node_id;
    // Plan 0.3 1.2: batch assignment is opt-in via header; legacy nodes (no
    // header) keep single-assignment semantics.
    let max_batch = headers
        .get("x-agentgrid-max-batch")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1);
    // Plan 533: degrade + touch + assign are coordinated in SchedulerService.
    // CP-managed egress proxy list for this node (global pool + node-scoped).
    // Failure must not break polling — an empty list means no update.
    // CP-pushed node config: egress proxies + adapter env (best-effort —
    // failures leave the node on its previous snapshot).
    let proxy_urls = state
        .store
        .proxy_urls_for(&req.node_id)
        .await
        .unwrap_or_default();
    let adapter_env = state
        .store
        .adapter_env_for(&req.node_id)
        .await
        .unwrap_or_default();
    let out = match state.scheduler.poll(&req, max_batch).await {
        Ok((_, batch)) if !batch.is_empty() => {
            let mut resp = PollResponse {
                assignment: None,
                assignments: batch,
                proxy_urls,
                adapter_env,
            };
            resp.assignment = Some(resp.assignments[0].clone());
            (StatusCode::OK, Json(resp))
        }
        Ok(_) => (
            StatusCode::OK,
            Json(PollResponse {
                assignment: None,
                assignments: Vec::new(),
                proxy_urls,
                adapter_env,
            }),
        ),
        Err(e) => {
            tracing::error!("poll failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(PollResponse {
                    assignment: None,
                    assignments: Vec::new(),
                    proxy_urls,
                    adapter_env,
                }),
            )
        }
    };
    POLL_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    POLL_DURATION_MS.fetch_add(
        start.elapsed().as_millis() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::sse_resume_after;

    fn header(v: &str) -> axum::http::HeaderValue {
        axum::http::HeaderValue::from_str(v).unwrap()
    }

    #[test]
    fn resume_uses_query_when_higher_than_header() {
        // Explicit after_ingest query wins over Last-Event-ID when newer.
        assert_eq!(
            sse_resume_after(Some(5), 0, Some(&header("2"))),
            (Some(5), 0)
        );
    }

    #[test]
    fn resume_uses_header_when_higher_than_query() {
        // Last-Event-ID promotes a reconnect that started at 0 up to last
        // ingest_id (Hardening P0 item 9: the header carries the global cursor).
        assert_eq!(sse_resume_after(None, 0, Some(&header("7"))), (Some(7), 0));
    }

    #[test]
    fn resume_takes_max_of_both() {
        assert_eq!(
            sse_resume_after(Some(3), 0, Some(&header("3"))),
            (Some(3), 0)
        );
    }

    #[test]
    fn resume_without_header_is_query() {
        assert_eq!(sse_resume_after(Some(9), 0, None), (Some(9), 0));
    }

    #[test]
    fn resume_ignores_non_numeric_header() {
        // A garbage Last-Event-ID falls back to the query (no gaps, no dup).
        assert_eq!(
            sse_resume_after(None, 0, Some(&header("garbage"))),
            (None, 0)
        );
    }

    #[test]
    fn resume_legacy_sequence_without_ingest_cursor() {
        // Pre-0037 client: no after_ingest, no Last-Event-ID — the per-attempt
        // after_sequence is passed through unchanged.
        assert_eq!(sse_resume_after(None, 42, None), (None, 42));
    }
}
