//! Task CRUD + events. Extracted from `store.rs`.

use super::{
    event_type_of, now_iso, page_limit, row_to_task_view, Store, DEFAULT_EVENT_PAGE, KEYSET_ORDER,
    KEYSET_PREDICATE,
};
use agentgrid_common::{CreateTaskRequest, TaskEvent, TaskStatus, TaskView};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

impl Store {
    /// GitHub webhook delivery dedup (audit CP-4): delivery is at-least-once,
    /// and every replay used to mint a fresh task — duplicate full agent runs
    /// for the same issue/CI failure/PR. Records the delivery GUID with
    /// INSERT OR IGNORE and returns false when the GUID was already seen
    /// (the handler then drops the replay). Uses the write txn (single
    /// writer) so two concurrent replays cannot both insert.
    pub async fn webhook_delivery_fresh(&self, guid: &str) -> Result<bool> {
        let n =
            sqlx::query("INSERT OR IGNORE INTO webhook_deliveries (guid, seen_at) VALUES (?, ?)")
                .bind(guid)
                .bind(now_iso())
                .execute(&self.pool)
                .await?
                .rows_affected();
        Ok(n == 1)
    }

    pub async fn create_task(&self, req: &CreateTaskRequest) -> Result<TaskView> {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        let timeout_secs = req.timeout_secs.unwrap_or(3600) as i64;
        sqlx::query(
            "INSERT INTO tasks (id, repository, prompt, adapter, requested_node_id, base_commit, parent_acp_session_id, network_mode, status, created_at, timeout_secs, validation_command, security_profile, group_id, agent_id, consensus_group_id, consensus_member, opencode_override) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .bind(&req.group_id)
        .bind(&req.agent_id)
        .bind(&req.consensus_group_id)
        .bind(&req.consensus_member)
        .bind(req.opencode_override.as_ref().map(|o| serde_json::to_string(o).unwrap_or_default()))
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
            group_id: req.group_id.clone(),
            agent_id: req.agent_id.clone(),
            consensus_group_id: None,
            consensus_member: None,
            opencode_override: req.opencode_override.clone(),
        })
    }

    pub async fn list_tasks(&self) -> Result<Vec<TaskView>> {
        // Hardening P2 item 20: server-side maximum limit so a client (or a huge
        // DB) cannot pull an unbounded row set in one request.
        self.list_tasks_filtered(None, None, None, None, Some(1000))
            .await
    }

    /// Audit X-C3: full-table status counts for /metrics. `list_tasks()`
    /// caps at MAX_TASKS oldest rows, which froze every task gauge and
    /// outcome counter (and silently skewed alerting) once the table grew
    /// past the cap.
    pub async fn task_status_counts(&self) -> Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT status, COUNT(*) FROM tasks GROUP BY status")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    /// Audit X-C3: durations (seconds) of the most recent terminal tasks —
    /// histogram input for /metrics, newest first so the window tracks live
    /// traffic instead of the oldest page.
    pub async fn recent_terminal_task_seconds(&self, limit: i64) -> Result<Vec<i64>> {
        let rows: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT finished_at, created_at FROM tasks \
             WHERE finished_at IS NOT NULL AND started_at IS NOT NULL \
             ORDER BY created_at DESC, id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (f, c) in rows {
            if let (Some(f), Some(c)) = (f, c) {
                if let (Ok(fdt), Ok(cdt)) = (
                    chrono::DateTime::parse_from_rfc3339(&f),
                    chrono::DateTime::parse_from_rfc3339(&c),
                ) {
                    out.push((fdt - cdt).num_seconds().max(0));
                }
            }
        }
        Ok(out)
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
        let limit = page_limit(limit);
        // Build the query with only the present filters as bound params. The
        // `node_id` filter joins the latest attempt's node via a correlated
        // subquery on `assigned_attempt_id`.
        let mut sql = String::from(
            "SELECT id, repository, prompt, adapter, status, created_at, finished_at, assigned_attempt_id, validation_command, error_code, requested_node_id, base_commit, parent_acp_session_id, network_mode, group_id, agent_id, \
                    (SELECT provenance FROM attempts WHERE task_id = tasks.id ORDER BY number DESC LIMIT 1) AS attempt_provenance, \
                    consensus_group_id, consensus_member, opencode_override \
             FROM tasks WHERE 1=1",
        );
        if after.is_some() {
            sql.push_str(KEYSET_PREDICATE);
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
        sql.push_str(KEYSET_ORDER);
        // audited: clauses are compile-time constants; values are bound
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
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

    /// Plan 1.3 (#6): full-text search over task prompt/repository via the
    /// FTS5 mirror (migration 0055), ranked by bm25, capped at 50 rows.
    pub async fn search_tasks(&self, query: &str) -> Result<Vec<TaskView>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        // FTS5 query syntax: wrap the query in quotes so user punctuation
        // does not break out of the tokenizer; a literal double quote inside
        // the query is dropped (cannot be escaped in a quoted phrase).
        let clean: String = q.chars().filter(|c| *c != '"').collect();
        let fts = format!("\"{clean}\"");
        let rows = sqlx::query(
            "SELECT tasks.id, tasks.repository, tasks.prompt, tasks.adapter, tasks.status, tasks.created_at, tasks.finished_at, \
                    tasks.assigned_attempt_id, tasks.validation_command, tasks.error_code, tasks.requested_node_id, \
                    tasks.base_commit, tasks.parent_acp_session_id, tasks.network_mode, tasks.group_id, tasks.agent_id, \
                    (SELECT provenance FROM attempts WHERE task_id = tasks.id ORDER BY number DESC LIMIT 1) AS attempt_provenance, \
                    tasks.consensus_group_id, tasks.consensus_member, tasks.opencode_override \
             FROM tasks JOIN tasks_fts ON tasks_fts.rowid = tasks.rowid \
             WHERE tasks_fts MATCH ? \
             ORDER BY bm25(tasks_fts) LIMIT 50",
        )
        .bind(&fts)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_task_view).collect())
    }

    /// Plan 1.3 (#13): list tags for a task.
    pub async fn list_tags(&self, task_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT tag FROM task_tags WHERE task_id = ? ORDER BY tag")
            .bind(task_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().filter_map(|r| r.try_get("tag").ok()).collect())
    }

    /// Plan 1.3 (#13): add a tag (idempotent — UNIQUE(pk) makes re-add a no-op).
    pub async fn add_tag(&self, task_id: &str, tag: &str) -> Result<()> {
        let tag = tag.trim();
        if tag.is_empty() {
            return Ok(());
        }
        sqlx::query("INSERT OR IGNORE INTO task_tags (task_id, tag, created_at) VALUES (?, ?, ?)")
            .bind(task_id)
            .bind(tag)
            .bind(now_iso())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Plan 1.3 (#13): remove a tag. Returns whether it existed.
    pub async fn remove_tag(&self, task_id: &str, tag: &str) -> Result<bool> {
        let r = sqlx::query("DELETE FROM task_tags WHERE task_id = ? AND tag = ?")
            .bind(task_id)
            .bind(tag)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn show_task(&self, id: &str) -> Result<Option<TaskView>> {
        let row = sqlx::query(
            "SELECT id, repository, prompt, adapter, status, created_at, finished_at, assigned_attempt_id, validation_command, error_code, requested_node_id, base_commit, parent_acp_session_id, network_mode, group_id, agent_id, \
                    (SELECT provenance FROM attempts WHERE task_id = tasks.id ORDER BY number DESC LIMIT 1) AS attempt_provenance, \
                    consensus_group_id, consensus_member, opencode_override \
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

    /// Age in seconds of the oldest `queued` task (plan 0.3 stage 0 metric);
    /// None when nothing is queued.
    pub async fn oldest_queued_age_secs(&self) -> Result<Option<f64>> {
        let row = sqlx::query(
            "SELECT (julianday('now') - julianday(MIN(created_at))) * 86400.0 AS age \
             FROM tasks WHERE status = 'queued'",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get::<Option<f64>, _>("age")?)
    }
}

/// Insert a queued task inside the caller's transaction. Same shape as
/// [`Store::create_task`]; shared by the atomic agent-budget path so the
/// budget check and the attributed insert commit together.
pub(crate) async fn insert_task_tx(
    req: &CreateTaskRequest,
    tx: &mut sqlx::SqliteConnection,
) -> Result<TaskView> {
    let id = Uuid::new_v4().to_string();
    let now = now_iso();
    let timeout_secs = req.timeout_secs.unwrap_or(3600) as i64;
    sqlx::query(
        "INSERT INTO tasks (id, repository, prompt, adapter, requested_node_id, base_commit, parent_acp_session_id, network_mode, status, created_at, timeout_secs, validation_command, security_profile, group_id, agent_id, consensus_group_id, consensus_member, opencode_override) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    .bind(&req.group_id)
    .bind(&req.agent_id)
    .bind(&req.consensus_group_id)
    .bind(&req.consensus_member)
    .bind(req.opencode_override.as_ref().map(|o| serde_json::to_string(o).unwrap_or_default()))
    .execute(tx)
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
        group_id: req.group_id.clone(),
        agent_id: req.agent_id.clone(),
        consensus_group_id: None,
        consensus_member: None,
        opencode_override: req.opencode_override.clone(),
    })
}
