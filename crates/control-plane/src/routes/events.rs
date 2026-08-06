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

pub async fn poll(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedNode>,
    Json(mut req): Json<PollRequest>,
) -> (StatusCode, Json<PollResponse>) {
    // The authenticated node id is the source of truth; ignore any client-supplied id.
    req.node_id = auth.node_id;
    // Plan 533: degrade + touch + assign are coordinated in SchedulerService.
    match state.scheduler.poll(&req).await {
        Ok((_, Some(assignment))) => (
            StatusCode::OK,
            Json(PollResponse {
                assignment: Some(assignment),
            }),
        ),
        Ok((_, None)) => (StatusCode::OK, Json(PollResponse { assignment: None })),
        Err(e) => {
            tracing::error!("poll failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(PollResponse { assignment: None }),
            )
        }
    }
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
