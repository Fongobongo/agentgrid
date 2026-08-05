//! Task event ingestion (idempotent, contiguous-prefix ack). Extracted from `store.rs`.

use super::{event_type_str, is_locked_err, now_iso, Store};
use agentgrid_common::IngestEventsRequest;
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

impl Store {
    pub async fn ingest_events(
        &self,
        attempt_id: &str,
        req: &IngestEventsRequest,
    ) -> Result<agentgrid_common::IngestEventsAck> {
        // Hardening P0 item 9: the global `ingest_id` counter makes every
        // ingest a write, so concurrent batches contend for the writer lock.
        // SQLite's busy_timeout does not cover `SQLITE_BUSY_SNAPSHOT` (deferred
        // BEGIN read-then-write), so retry the transaction body on "database is
        // locked" with a short backoff — a node pushing many attempts
        // concurrently must never see intermittent 500s. 12 attempts × up to
        // 600ms covers the worst burst without stalling a normal ingest.
        for attempt in 0..12u32 {
            match self.ingest_events_inner(attempt_id, req).await {
                Ok(ack) => return Ok(ack),
                Err(e) if is_locked_err(&e) && attempt < 11 => {
                    tokio::time::sleep(std::time::Duration::from_millis(10 + 50 * attempt as u64))
                        .await;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("retry loop always returns")
    }

    async fn ingest_events_inner(
        &self,
        attempt_id: &str,
        req: &IngestEventsRequest,
    ) -> Result<agentgrid_common::IngestEventsAck> {
        use agentgrid_common::IngestEventsAck;
        let mut tx = self.pool.begin().await?;
        let attempt = sqlx::query("SELECT task_id, status FROM attempts WHERE id = ?")
            .bind(attempt_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(attempt) = attempt else {
            let _ = tx.rollback().await;
            return Ok(IngestEventsAck::default());
        };
        let task_id: String = attempt.try_get("task_id")?;
        let attempt_status: String = attempt.try_get("status")?;

        // Hardening P1 item 14: do not accept events for a terminal/lost
        // attempt. A node that restarts after we marked its attempt lost must
        // not append to (or resurrect) a finished attempt's event stream.
        if matches!(
            attempt_status.as_str(),
            "succeeded" | "failed" | "cancelled" | "lost"
        ) {
            let _ = tx.rollback().await;
            return Ok(IngestEventsAck::default());
        }

        if attempt_status == "assigned" {
            sqlx::query("UPDATE attempts SET status = 'running' WHERE id = ?")
                .bind(attempt_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE tasks SET status = 'running', started_at = ? WHERE id = ?")
                .bind(now_iso())
                .bind(&task_id)
                .execute(&mut *tx)
                .await?;
        }

        let mut accepted = 0u64;
        for ev in &req.events {
            let payload = serde_json::to_string(&ev.payload)?;
            let id = Uuid::new_v4().to_string();
            // Hardening P0 item 9: allocate the global ingest cursor from the
            // single-row counter inside the same transaction, so every inserted
            // event gets a strictly monotonic id ordered across attempts.
            // Duplicate redeliveries consume a counter value but land nowhere
            // (ON CONFLICT DO NOTHING) — the cursor stays monotonic, not
            // necessarily gap-free, which is all the read path requires.
            let ingest_id: i64 = sqlx::query_scalar(
                "UPDATE event_ingest_counter SET next_val = next_val + 1 \
                 WHERE id = 1 RETURNING next_val",
            )
            .fetch_one(&mut *tx)
            .await?;
            let r = sqlx::query(
                "INSERT INTO task_events (id, attempt_id, sequence, type, payload, created_at, ingest_id) \
                 VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(attempt_id, sequence) DO NOTHING",
            )
            .bind(&id)
            .bind(attempt_id)
            .bind(ev.sequence as i64)
            .bind(event_type_str(ev.r#type))
            .bind(&payload)
            .bind(now_iso())
            .bind(ingest_id)
            .execute(&mut *tx)
            .await?;
            accepted += r.rows_affected();
        }
        // Hardening P1 item 14: report the contiguous event-sequence prefix we
        // hold for this attempt (1..=N with no gaps), so a client can detect a
        // gap when the durable outbox redelivers. Computed after commit so the
        // prefix reflects the rows this batch landed; cheap for typical event
        // counts, but O(rows) load — TODO: switch to a recursive CTE / tracked
        // cursor once an attempt surfaces millions of events.
        tx.commit().await?;
        let highest_contiguous = self.contiguous_event_prefix(attempt_id).await?;
        Ok(IngestEventsAck {
            accepted,
            highest_contiguous_sequence: highest_contiguous,
        })
    }

    /// Largest `N` such that sequences `1..=N` all exist in `task_events` for
    /// this attempt (the contiguous prefix). `0` if no events. O(rows) load —
    /// see `ingest_events` for the ceiling.
    async fn contiguous_event_prefix(&self, attempt_id: &str) -> Result<Option<u64>> {
        let rows: Vec<i64> = sqlx::query_scalar(
            "SELECT sequence FROM task_events WHERE attempt_id = ? GROUP BY sequence ORDER BY sequence",
        )
        .bind(attempt_id)
        .fetch_all(&self.pool)
        .await?;
        let mut prev = 0i64;
        for s in rows {
            if s != prev + 1 {
                break;
            }
            prev = s;
        }
        if prev <= 0 {
            Ok(None)
        } else {
            Ok(Some(prev as u64))
        }
    }
}
