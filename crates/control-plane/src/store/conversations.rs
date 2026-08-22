//! Stateful multi-turn conversations. Extracted from `store.rs`.

use super::{now_iso, Store};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

impl Store {
    // ----- conversations (stateful multi-turn chat) -----

    pub async fn create_conversation(
        &self,
        adapter: &str,
        repository: &str,
    ) -> Result<agentgrid_common::Conversation> {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO conversations (id, adapter, repository, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(adapter)
        .bind(repository)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(agentgrid_common::Conversation {
            id,
            adapter: adapter.to_string(),
            repository: repository.to_string(),
            created_at: now,
        })
    }

    pub async fn get_conversation(
        &self,
        id: &str,
    ) -> Result<Option<agentgrid_common::Conversation>> {
        let row = sqlx::query(
            "SELECT id, adapter, repository, created_at FROM conversations WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| agentgrid_common::Conversation {
            id: r.try_get("id").unwrap_or_default(),
            adapter: r.try_get("adapter").unwrap_or_default(),
            repository: r.try_get("repository").unwrap_or_default(),
            created_at: r.try_get("created_at").unwrap_or_default(),
        }))
    }

    /// Append a message; returns its sequence number. `task_id` is the task that
    /// produced (assistant) or carried (user) the message.
    pub async fn append_conversation_message(
        &self,
        conversation_id: &str,
        role: &str,
        content: &str,
        task_id: Option<&str>,
    ) -> Result<i64> {
        let now = now_iso();
        // Hardening P2 item 21: allocate the message sequence atomically inside
        // a single INSERT (subquery + INSERT are one statement in SQLite, so
        // the MAX/INSERT pair cannot interleave across concurrent appends).
        // The UNIQUE(conversation_id, seq) index is the DB-side backstop.
        let id = Uuid::new_v4().to_string();
        let seq: i64 = sqlx::query_scalar(
            "INSERT INTO conversation_messages (id, conversation_id, seq, role, content, task_id, created_at) \
             VALUES (?, ?, (SELECT COALESCE(MAX(seq), 0) + 1 FROM conversation_messages WHERE conversation_id = ?), ?, ?, ?, ?) \
             RETURNING seq",
        )
        .bind(&id)
        .bind(conversation_id)
        .bind(conversation_id)
        .bind(role)
        .bind(content)
        .bind(task_id)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok(seq)
    }

    pub async fn list_conversation_messages(
        &self,
        conversation_id: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<agentgrid_common::ConversationMessage>> {
        let limit = limit.clamp(1, 1000);
        let rows = sqlx::query(
            "SELECT seq, role, content, task_id, created_at FROM conversation_messages \
             WHERE conversation_id = ? AND seq > ? ORDER BY seq ASC LIMIT ?",
        )
        .bind(conversation_id)
        .bind(after_seq)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| agentgrid_common::ConversationMessage {
                seq: r.try_get("seq").unwrap_or_default(),
                role: r.try_get("role").unwrap_or_default(),
                content: r.try_get("content").unwrap_or_default(),
                task_id: r.try_get("task_id").unwrap_or_default(),
                created_at: r.try_get("created_at").unwrap_or_default(),
            })
            .collect())
    }

    /// Audit X-C6: attach the spawned task id to the already-persisted user
    /// turn. The message is appended BEFORE `create_task` so a failed task
    /// creation can no longer leave a live agent running on an unlogged turn.
    pub async fn set_conversation_message_task(
        &self,
        conversation_id: &str,
        seq: i64,
        task_id: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE conversation_messages SET task_id = ? \
             WHERE conversation_id = ? AND seq = ?",
        )
        .bind(task_id)
        .bind(conversation_id)
        .bind(seq)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Stage 11.5: the most recent ACP session id produced by a finished task
    /// in this conversation, so the next task can resume it. `None` when there
    /// is no resumable session (first turn, or the prior attempt was not ACP).
    pub async fn last_conversation_acp_session(
        &self,
        conversation_id: &str,
    ) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT a.acp_session_id AS sid \
             FROM conversation_messages m \
             JOIN attempts a ON a.task_id = m.task_id \
             WHERE m.conversation_id = ? AND a.acp_session_id IS NOT NULL \
             ORDER BY m.seq DESC LIMIT 1",
        )
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|r| r.try_get::<Option<String>, _>("sid").ok().flatten()))
    }
}
