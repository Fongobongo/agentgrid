//! Task CRUD + events. Extracted from `store.rs`.

use super::{event_type_of, now_iso, row_to_task_view, Store, DEFAULT_EVENT_PAGE};
use agentgrid_common::{CreateTaskRequest, TaskEvent, TaskStatus, TaskView};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

impl Store {
    pub async fn create_task(&self, req: &CreateTaskRequest) -> Result<TaskView> {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        let timeout_secs = req.timeout_secs.unwrap_or(3600) as i64;
        sqlx::query(
            "INSERT INTO tasks (id, repository, prompt, adapter, requested_node_id, base_commit, parent_acp_session_id, network_mode, status, created_at, timeout_secs, validation_command, security_profile) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&req.repository)
        .bind(&req.prompt)
        .bind(&req.adapter)
        .bind(&req.requested_node_id)
        .bind(&req.base_commit)
        .bind(&req.parent_acp_session_id)
        .bind(&req.network_mode)
        .bind(&now)
        .bind(timeout_secs)
        .bind(&req.validation_command)
        .bind(&req.security_profile)
        .execute(&self.pool)
        .await?;
        Ok(TaskView {
            id,
            repository: req.repository.clone(),
            prompt: req.prompt.clone(),
            adapter: req.adapter.clone(),
            status: TaskStatus::Queued,
            created_at: now,
            finished_at: None,
            assigned_attempt_id: None,
            validation_command: req.validation_command.clone(),
            error_code: None,
            requested_node_id: req.requested_node_id.clone(),
            base_commit: req.base_commit.clone(),
            parent_acp_session_id: req.parent_acp_session_id.clone(),
            network_mode: req.network_mode.clone(),
            security_profile: req.security_profile.clone(),
        })
    }

    pub async fn list_tasks(&self) -> Result<Vec<TaskView>> {
        // Hardening P2 item 20: server-side maximum limit so a client (or a huge
        // DB) cannot pull an unbounded row set in one request.
        self.list_tasks_filtered(None, None, None, None, Some(1000))
            .await
    }

    /// Hardening P2 item 20: list tasks with optional server-side filters
    /// (`status`, `repository`, `node_id`) plus the same row cap. Each filter
    /// is exact match; `None` means no predicate. Symbol/leftover lexic of
    /// `active_attempts` are not involved.
    ///
    /// Cursor pagination (hardening P2 item 20): `after` is a keyset cursor
    /// `(created_at, id)` — rows strictly after it are returned (stable order
    /// by `created_at, id` even when timestamps collide). `limit` caps the
    /// page (server-enforced ceiling).
    pub async fn list_tasks_filtered(
        &self,
        status: Option<&str>,
        repository: Option<&str>,
        node_id: Option<&str>,
        after: Option<(String, String)>,
        limit: Option<u64>,
    ) -> Result<Vec<TaskView>> {
        const MAX_TASKS: i64 = 1000;
        let limit = limit.unwrap_or(100).min(MAX_TASKS as u64) as i64;
        // Build the query with only the present filters as bound params. The
        // `node_id` filter joins the latest attempt's node via a correlated
        // subquery on `assigned_attempt_id`.
        let mut sql = String::from(
            "SELECT id, repository, prompt, adapter, status, created_at, finished_at, assigned_attempt_id, validation_command, error_code, requested_node_id, base_commit, parent_acp_session_id, network_mode, \
                    (SELECT provenance FROM attempts WHERE task_id = tasks.id ORDER BY number DESC LIMIT 1) AS attempt_provenance \
             FROM tasks WHERE 1=1",
        );
        if after.is_some() {
            sql.push_str(" AND (created_at > ? OR (created_at = ? AND id > ?))");
        }
        if status.is_some() {
            sql.push_str(" AND status = ?");
        }
        if repository.is_some() {
            sql.push_str(" AND repository = ?");
        }
        if node_id.is_some() {
            sql.push_str(" AND assigned_attempt_id IN (SELECT id FROM attempts WHERE node_id = ?)");
        }
        sql.push_str(" ORDER BY created_at ASC, id ASC LIMIT ?");
        let mut q = sqlx::query(&sql);
        if let Some((created_at, id)) = &after {
            // The keyset predicate has three placeholders: created_at > ?, and
            // the tie-break (created_at = ? AND id > ?).
            q = q.bind(created_at).bind(created_at).bind(id);
        }
        if let Some(s) = status {
            q = q.bind(s);
        }
        if let Some(r) = repository {
            q = q.bind(r);
        }
        if let Some(n) = node_id {
            q = q.bind(n);
        }
        let rows = q.bind(limit).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(row_to_task_view).collect())
    }

    pub async fn show_task(&self, id: &str) -> Result<Option<TaskView>> {
        let row = sqlx::query(
            "SELECT id, repository, prompt, adapter, status, created_at, finished_at, assigned_attempt_id, validation_command, error_code, requested_node_id, base_commit, parent_acp_session_id, \
                    (SELECT provenance FROM attempts WHERE task_id = tasks.id ORDER BY number DESC LIMIT 1) AS attempt_provenance \
             FROM tasks WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_task_view))
    }

    /// Hardening P0 item 9: read events for a task ordered by the global
    /// `ingest_id` cursor. `after_ingest` resumes after that global cursor;
    /// `limit` caps the page (server enforces a hard cap). The legacy
    /// `after_sequence` cursor is honoured as a per-attempt filter so pre-0037
    /// clients keep working; results still carry `ingest_id`.
    pub async fn get_events(
        &self,
        task_id: &str,
        after_ingest: Option<u64>,
        after_sequence: u64,
        limit: Option<u64>,
    ) -> Result<Vec<TaskEvent>> {
        let limit = limit.unwrap_or(DEFAULT_EVENT_PAGE).min(DEFAULT_EVENT_PAGE) as i64;
        let rows = match after_ingest {
            Some(after) => sqlx::query(
                "SELECT e.attempt_id, e.sequence, e.type, e.payload, e.created_at, e.ingest_id \
                     FROM task_events e \
                     JOIN attempts a ON a.id = e.attempt_id \
                     WHERE a.task_id = ? AND e.ingest_id > ? \
                     ORDER BY e.ingest_id ASC LIMIT ?",
            )
            .bind(task_id)
            .bind(after as i64)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?,
            None => {
                // Legacy per-attempt cursor: filter each attempt's sequence.
                sqlx::query(
                    "SELECT e.attempt_id, e.sequence, e.type, e.payload, e.created_at, e.ingest_id \
                     FROM task_events e \
                     JOIN attempts a ON a.id = e.attempt_id \
                     WHERE a.task_id = ? AND e.sequence > ? \
                     ORDER BY e.ingest_id ASC LIMIT ?",
                )
                .bind(task_id)
                .bind(after_sequence as i64)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
        };
        let mut events = Vec::with_capacity(rows.len());
        for r in rows {
            let payload_text: String = r.try_get("payload")?;
            events.push(TaskEvent {
                attempt_id: r.try_get("attempt_id")?,
                sequence: r.try_get::<i64, _>("sequence")? as u64,
                r#type: event_type_of(&r.try_get::<String, _>("type")?),
                payload: serde_json::from_str(&payload_text).unwrap_or(serde_json::Value::Null),
                created_at: r.try_get("created_at")?,
                ingest_id: r.try_get::<i64, _>("ingest_id")? as u64,
            });
        }
        Ok(events)
    }
}
