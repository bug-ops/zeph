// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::error::MemoryError;
use crate::sqlite::SqliteStore;
use crate::types::ConversationId;

pub struct AcpSessionEvent {
    pub event_type: String,
    pub payload: String,
    pub created_at: String,
}

pub struct AcpSessionInfo {
    pub id: String,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: i64,
}

impl SqliteStore {
    /// Create a new ACP session record.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn create_acp_session(&self, session_id: &str) -> Result<(), MemoryError> {
        sqlx::query("INSERT OR IGNORE INTO acp_sessions (id) VALUES (?)")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Persist a single ACP session event.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn save_acp_event(
        &self,
        session_id: &str,
        event_type: &str,
        payload: &str,
    ) -> Result<(), MemoryError> {
        sqlx::query(
            "INSERT INTO acp_session_events (session_id, event_type, payload) VALUES (?, ?, ?)",
        )
        .bind(session_id)
        .bind(event_type)
        .bind(payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load all events for an ACP session in insertion order.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn load_acp_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<AcpSessionEvent>, MemoryError> {
        let rows = sqlx::query_as::<_, (String, String, String)>(
            "SELECT event_type, payload, created_at FROM acp_session_events WHERE session_id = ? ORDER BY id",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(event_type, payload, created_at)| AcpSessionEvent {
                event_type,
                payload,
                created_at,
            })
            .collect())
    }

    /// Delete an ACP session and its events (cascade).
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn delete_acp_session(&self, session_id: &str) -> Result<(), MemoryError> {
        sqlx::query("DELETE FROM acp_sessions WHERE id = ?")
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// List ACP sessions ordered by last activity descending.
    ///
    /// Includes title, `updated_at`, and message count per session.
    /// Pass `limit = 0` for unlimited results.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn list_acp_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<AcpSessionInfo>, MemoryError> {
        // LIMIT -1 in SQLite means no limit; cast limit=0 sentinel to -1.
        #[allow(clippy::cast_possible_wrap)]
        let sql_limit: i64 = if limit == 0 { -1 } else { limit as i64 };
        let rows = sqlx::query_as::<_, (String, Option<String>, String, String, i64)>(
            "SELECT s.id, s.title, s.created_at, s.updated_at, \
             (SELECT COUNT(*) FROM acp_session_events WHERE session_id = s.id) AS message_count \
             FROM acp_sessions s \
             ORDER BY s.updated_at DESC \
             LIMIT ?",
        )
        .bind(sql_limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, title, created_at, updated_at, message_count)| AcpSessionInfo {
                    id,
                    title,
                    created_at,
                    updated_at,
                    message_count,
                },
            )
            .collect())
    }

    /// Fetch metadata for a single ACP session.
    ///
    /// Returns `None` if the session does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn get_acp_session_info(
        &self,
        session_id: &str,
    ) -> Result<Option<AcpSessionInfo>, MemoryError> {
        let row = sqlx::query_as::<_, (String, Option<String>, String, String, i64)>(
            "SELECT s.id, s.title, s.created_at, s.updated_at, \
             (SELECT COUNT(*) FROM acp_session_events WHERE session_id = s.id) AS message_count \
             FROM acp_sessions s \
             WHERE s.id = ?",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(id, title, created_at, updated_at, message_count)| AcpSessionInfo {
                id,
                title,
                created_at,
                updated_at,
                message_count,
            },
        ))
    }

    /// Insert multiple events for a session inside a single transaction.
    ///
    /// Atomically writes all events or none. More efficient than individual inserts
    /// for bulk import use cases.
    ///
    /// # Errors
    ///
    /// Returns an error if the transaction or any insert fails.
    pub async fn import_acp_events(
        &self,
        session_id: &str,
        events: &[(&str, &str)],
    ) -> Result<(), MemoryError> {
        let mut tx = self.pool.begin().await?;
        for (event_type, payload) in events {
            sqlx::query(
                "INSERT INTO acp_session_events (session_id, event_type, payload) VALUES (?, ?, ?)",
            )
            .bind(session_id)
            .bind(event_type)
            .bind(payload)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Update the title of an ACP session.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn update_session_title(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<(), MemoryError> {
        sqlx::query("UPDATE acp_sessions SET title = ? WHERE id = ?")
            .bind(title)
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Check whether an ACP session record exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn acp_session_exists(&self, session_id: &str) -> Result<bool, MemoryError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM acp_sessions WHERE id = ?")
            .bind(session_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(count > 0)
    }

    /// Create a new ACP session record with an associated conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn create_acp_session_with_conversation(
        &self,
        session_id: &str,
        conversation_id: ConversationId,
    ) -> Result<(), MemoryError> {
        sqlx::query("INSERT OR IGNORE INTO acp_sessions (id, conversation_id) VALUES (?, ?)")
            .bind(session_id)
            .bind(conversation_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Get the conversation ID associated with an ACP session.
    ///
    /// Returns `None` if the session has no conversation mapping (legacy session).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn get_acp_session_conversation_id(
        &self,
        session_id: &str,
    ) -> Result<Option<ConversationId>, MemoryError> {
        let row: Option<(Option<ConversationId>,)> =
            sqlx::query_as("SELECT conversation_id FROM acp_sessions WHERE id = ?")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(cid,)| cid))
    }

    /// Update the conversation mapping for an ACP session.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn set_acp_session_conversation_id(
        &self,
        session_id: &str,
        conversation_id: ConversationId,
    ) -> Result<(), MemoryError> {
        sqlx::query("UPDATE acp_sessions SET conversation_id = ? WHERE id = ?")
            .bind(conversation_id)
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Copy all messages from one conversation to another, preserving order.
    ///
    /// Summaries are intentionally NOT copied: their `first_message_id`/`last_message_id`
    /// reference message IDs from the source conversation which differ from the new IDs
    /// assigned to the copied messages, making the compaction cursor incorrect. The forked
    /// session inherits the full message history and builds its own compaction state from
    /// scratch. Other per-conversation state also excluded: embeddings (re-indexed on demand),
    /// deferred tool summaries (treated as fresh context budget).
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn copy_conversation(
        &self,
        source: ConversationId,
        target: ConversationId,
    ) -> Result<(), MemoryError> {
        let mut tx = self.pool.begin().await?;

        // Copy messages in order. Only columns present across all migrations are included;
        // per-message auto-fields (id, created_at, last_accessed, access_count, qdrant_cleaned)
        // are excluded so they are generated fresh for the target conversation.
        sqlx::query(
            "INSERT INTO messages \
                (conversation_id, role, content, parts, agent_visible, user_visible, compacted_at, deleted_at) \
             SELECT ?, role, content, parts, agent_visible, user_visible, compacted_at, deleted_at \
             FROM messages WHERE conversation_id = ? ORDER BY id",
        )
        .bind(target)
        .bind(source)
        .execute(&mut *tx)
        .await?;

        // Summaries are NOT copied — their message ID boundaries reference the source
        // conversation and would corrupt the compaction cursor in the forked session.
        // The forked session builds compaction state from its own messages.

        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_store() -> SqliteStore {
        SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new")
    }

    #[tokio::test]
    async fn create_and_exists() {
        let store = make_store().await;
        store.create_acp_session("sess-1").await.unwrap();
        assert!(store.acp_session_exists("sess-1").await.unwrap());
        assert!(!store.acp_session_exists("sess-2").await.unwrap());
    }

    #[tokio::test]
    async fn save_and_load_events() {
        let store = make_store().await;
        store.create_acp_session("sess-1").await.unwrap();
        store
            .save_acp_event("sess-1", "user_message", "hello")
            .await
            .unwrap();
        store
            .save_acp_event("sess-1", "agent_message", "world")
            .await
            .unwrap();

        let events = store.load_acp_events("sess-1").await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "user_message");
        assert_eq!(events[0].payload, "hello");
        assert_eq!(events[1].event_type, "agent_message");
        assert_eq!(events[1].payload, "world");
    }

    #[tokio::test]
    async fn delete_cascades_events() {
        let store = make_store().await;
        store.create_acp_session("sess-1").await.unwrap();
        store
            .save_acp_event("sess-1", "user_message", "hello")
            .await
            .unwrap();
        store.delete_acp_session("sess-1").await.unwrap();

        assert!(!store.acp_session_exists("sess-1").await.unwrap());
        let events = store.load_acp_events("sess-1").await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn load_events_empty_for_unknown() {
        let store = make_store().await;
        let events = store.load_acp_events("no-such").await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn list_sessions_includes_title_and_message_count() {
        let store = make_store().await;
        store.create_acp_session("sess-b").await.unwrap();

        // Sleep so that sess-a's events land in a different second than sess-b's
        // created_at, making the updated_at DESC ordering deterministic.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        store.create_acp_session("sess-a").await.unwrap();
        store.save_acp_event("sess-a", "user", "hi").await.unwrap();
        store
            .save_acp_event("sess-a", "agent", "hello")
            .await
            .unwrap();
        store
            .update_session_title("sess-a", "My Chat")
            .await
            .unwrap();

        let sessions = store.list_acp_sessions(100).await.unwrap();
        // sess-a has events so updated_at is newer — should be first
        assert_eq!(sessions[0].id, "sess-a");
        assert_eq!(sessions[0].title.as_deref(), Some("My Chat"));
        assert_eq!(sessions[0].message_count, 2);

        // sess-b has no events
        let b = sessions.iter().find(|s| s.id == "sess-b").unwrap();
        assert!(b.title.is_none());
        assert_eq!(b.message_count, 0);
    }

    #[tokio::test]
    async fn list_sessions_respects_limit() {
        let store = make_store().await;
        for i in 0..5u8 {
            store
                .create_acp_session(&format!("sess-{i}"))
                .await
                .unwrap();
        }
        let sessions = store.list_acp_sessions(3).await.unwrap();
        assert_eq!(sessions.len(), 3);
    }

    #[tokio::test]
    async fn list_sessions_limit_one_boundary() {
        let store = make_store().await;
        for i in 0..3u8 {
            store
                .create_acp_session(&format!("sess-{i}"))
                .await
                .unwrap();
        }
        let sessions = store.list_acp_sessions(1).await.unwrap();
        assert_eq!(sessions.len(), 1);
    }

    #[tokio::test]
    async fn list_sessions_unlimited_when_zero() {
        let store = make_store().await;
        for i in 0..5u8 {
            store
                .create_acp_session(&format!("sess-{i}"))
                .await
                .unwrap();
        }
        let sessions = store.list_acp_sessions(0).await.unwrap();
        assert_eq!(sessions.len(), 5);
    }

    #[tokio::test]
    async fn get_acp_session_info_returns_none_for_missing() {
        let store = make_store().await;
        let info = store.get_acp_session_info("no-such").await.unwrap();
        assert!(info.is_none());
    }

    #[tokio::test]
    async fn get_acp_session_info_returns_data() {
        let store = make_store().await;
        store.create_acp_session("sess-x").await.unwrap();
        store
            .save_acp_event("sess-x", "user", "hello")
            .await
            .unwrap();
        store.update_session_title("sess-x", "Test").await.unwrap();

        let info = store.get_acp_session_info("sess-x").await.unwrap().unwrap();
        assert_eq!(info.id, "sess-x");
        assert_eq!(info.title.as_deref(), Some("Test"));
        assert_eq!(info.message_count, 1);
    }

    #[tokio::test]
    async fn updated_at_trigger_fires_on_event_insert() {
        let store = make_store().await;
        store.create_acp_session("sess-t").await.unwrap();

        let before = store
            .get_acp_session_info("sess-t")
            .await
            .unwrap()
            .unwrap()
            .updated_at
            .clone();

        // Small sleep so datetime('now') differs
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        store
            .save_acp_event("sess-t", "user", "ping")
            .await
            .unwrap();

        let after = store
            .get_acp_session_info("sess-t")
            .await
            .unwrap()
            .unwrap()
            .updated_at;

        assert!(
            after > before,
            "updated_at should increase after event insert: before={before} after={after}"
        );
    }

    #[tokio::test]
    async fn create_session_with_conversation_and_retrieve() {
        let store = make_store().await;
        let cid = store.create_conversation().await.unwrap();
        store
            .create_acp_session_with_conversation("sess-1", cid)
            .await
            .unwrap();
        let retrieved = store
            .get_acp_session_conversation_id("sess-1")
            .await
            .unwrap();
        assert_eq!(retrieved, Some(cid));
    }

    #[tokio::test]
    async fn get_conversation_id_returns_none_for_legacy_session() {
        let store = make_store().await;
        store.create_acp_session("legacy").await.unwrap();
        let cid = store
            .get_acp_session_conversation_id("legacy")
            .await
            .unwrap();
        assert!(cid.is_none());
    }

    #[tokio::test]
    async fn get_conversation_id_returns_none_for_missing_session() {
        let store = make_store().await;
        let cid = store
            .get_acp_session_conversation_id("no-such")
            .await
            .unwrap();
        assert!(cid.is_none());
    }

    #[tokio::test]
    async fn set_conversation_id_updates_existing_session() {
        let store = make_store().await;
        store.create_acp_session("sess-2").await.unwrap();
        let cid = store.create_conversation().await.unwrap();
        store
            .set_acp_session_conversation_id("sess-2", cid)
            .await
            .unwrap();
        let retrieved = store
            .get_acp_session_conversation_id("sess-2")
            .await
            .unwrap();
        assert_eq!(retrieved, Some(cid));
    }

    #[tokio::test]
    async fn copy_conversation_copies_messages_in_order() {
        use zeph_llm::provider::Role;
        let store = make_store().await;
        let src = store.create_conversation().await.unwrap();
        store.save_message(src, "user", "hello").await.unwrap();
        store.save_message(src, "assistant", "world").await.unwrap();

        let dst = store.create_conversation().await.unwrap();
        store.copy_conversation(src, dst).await.unwrap();

        let msgs = store.load_history(dst, 100).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].content, "hello");
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[1].content, "world");
    }

    #[tokio::test]
    async fn copy_conversation_empty_source_is_noop() {
        let store = make_store().await;
        let src = store.create_conversation().await.unwrap();
        let dst = store.create_conversation().await.unwrap();
        store.copy_conversation(src, dst).await.unwrap();
        let msgs = store.load_history(dst, 100).await.unwrap();
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn copy_conversation_does_not_copy_summaries() {
        // Summaries are intentionally excluded because their first/last_message_id
        // boundaries would reference source message IDs, corrupting the compaction cursor.
        let store = make_store().await;
        let src = store.create_conversation().await.unwrap();
        store.save_message(src, "user", "hello").await.unwrap();
        // Insert a summary directly so we can verify it is not copied.
        sqlx::query(
            "INSERT INTO summaries (conversation_id, content, first_message_id, last_message_id, token_estimate) \
             VALUES (?, 'summary text', 1, 1, 10)",
        )
        .bind(src)
        .execute(&store.pool)
        .await
        .unwrap();

        let dst = store.create_conversation().await.unwrap();
        store.copy_conversation(src, dst).await.unwrap();

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM summaries WHERE conversation_id = ?")
                .bind(dst)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(
            count, 0,
            "summaries must not be copied to forked conversation"
        );
    }

    #[tokio::test]
    async fn concurrent_sessions_get_distinct_conversation_ids() {
        let store = make_store().await;
        let cid1 = store.create_conversation().await.unwrap();
        let cid2 = store.create_conversation().await.unwrap();
        store
            .create_acp_session_with_conversation("sess-a", cid1)
            .await
            .unwrap();
        store
            .create_acp_session_with_conversation("sess-b", cid2)
            .await
            .unwrap();

        let retrieved1 = store
            .get_acp_session_conversation_id("sess-a")
            .await
            .unwrap();
        let retrieved2 = store
            .get_acp_session_conversation_id("sess-b")
            .await
            .unwrap();

        assert!(retrieved1.is_some());
        assert!(retrieved2.is_some());
        assert_ne!(
            retrieved1, retrieved2,
            "concurrent sessions must get distinct conversation_ids"
        );
    }
}
