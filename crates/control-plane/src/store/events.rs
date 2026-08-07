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
        let mut tx = self.write_txn().await?;
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
        // Plan 0.3 item 1.4: allocate the batch's ingest-id block with ONE
        // counter bump (was: one per event). Duplicates still consume cursor
        // values and land nowhere (ON CONFLICT DO NOTHING) — the cursor stays
        // monotonic, not necessarily gap-free, which is all the read path
        // requires.
        let batch_n = req.events.len() as i64;
        let block_last: i64 = if batch_n > 0 {
            sqlx::query_scalar(
                "UPDATE event_ingest_counter SET next_val = next_val + ? \
                 WHERE id = 1 RETURNING next_val",
            )
            .bind(batch_n)
            .fetch_one(&mut *tx)
            .await?
        } else {
            0
        };
        let block_first = block_last - batch_n + 1;
        for (i, ev) in req.events.iter().enumerate() {
            let payload = serde_json::to_string(&ev.payload)?;
            let id = Uuid::new_v4().to_string();
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
            .bind(block_first + i as i64)
            .execute(&mut *tx)
            .await?;
            accepted += r.rows_affected();
        }
        // Hardening P1 item 14: report the contiguous event-sequence prefix we
        // hold for this attempt (1..=N with no gaps), so a client can detect a
        // gap when the durable outbox redelivers. Computed after commit so the
        // prefix reflects the rows this batch landed; server-side window query
        // (plan 0.3 1.4), so no per-row transfer.
        tx.commit().await?;
        let highest_contiguous = self.contiguous_event_prefix(attempt_id).await?;
        Ok(IngestEventsAck {
            accepted,
            // Some(..) marks a live attempt (0 = no contiguous prefix yet);
            // the default-ack path (None) is reserved for gone/terminal
            // attempts so the route maps those — and only those — to 404.
            highest_contiguous_sequence: highest_contiguous.or(Some(0)),
        })
    }

    /// Largest `N` such that sequences `1..=N` all exist in `task_events` for
    /// this attempt (the contiguous prefix). `None` if no events. Computed
    /// server-side with a window function (plan 0.3 1.4): on the distinct
    /// sorted sequences, `sequence <> row_number()` first holds at the first
    /// gap, whose `sequence - 1` is the prefix; with no gap the prefix is
    /// `MAX(sequence)`.
    async fn contiguous_event_prefix(&self, attempt_id: &str) -> Result<Option<u64>> {
        let prefix: Option<i64> = sqlx::query_scalar(
            "WITH ds AS (SELECT DISTINCT sequence FROM task_events WHERE attempt_id = ?), \
                    numbered AS (SELECT sequence, ROW_NUMBER() OVER (ORDER BY sequence) AS rn FROM ds) \
             SELECT CASE WHEN (SELECT COUNT(*) FROM ds) = 0 THEN NULL \
                         ELSE COALESCE((SELECT MIN(rn) - 1 FROM numbered WHERE sequence <> rn), \
                                       (SELECT MAX(sequence) FROM numbered)) END",
        )
        .bind(attempt_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(prefix.map(|p| p as u64))
    }
}
