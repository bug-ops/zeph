use zeph_llm::provider::{Message, MessagePart, Role};

use super::SqliteStore;
use crate::error::MemoryError;

fn parse_role(s: &str) -> Role {
    match s {
        "assistant" => Role::Assistant,
        "system" => Role::System,
        _ => Role::User,
    }
}

#[must_use]
pub fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

impl SqliteStore {
    /// Create a new conversation and return its ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn create_conversation(&self) -> Result<i64, MemoryError> {
        let row: (i64,) = sqlx::query_as("INSERT INTO conversations DEFAULT VALUES RETURNING id")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    /// Save a message to the given conversation and return the message ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn save_message(
        &self,
        conversation_id: i64,
        role: &str,
        content: &str,
    ) -> Result<i64, MemoryError> {
        self.save_message_with_parts(conversation_id, role, content, "[]")
            .await
    }

    /// Save a message with structured parts JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn save_message_with_parts(
        &self,
        conversation_id: i64,
        role: &str,
        content: &str,
        parts_json: &str,
    ) -> Result<i64, MemoryError> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO messages (conversation_id, role, content, parts) VALUES (?, ?, ?, ?) RETURNING id",
        )
        .bind(conversation_id)
        .bind(role)
        .bind(content)
        .bind(parts_json)
        .fetch_one(&self.pool)
        .await
        ?;
        Ok(row.0)
    }

    /// Load the most recent messages for a conversation, up to `limit`.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_history(
        &self,
        conversation_id: i64,
        limit: u32,
    ) -> Result<Vec<Message>, MemoryError> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT role, content, parts FROM (\
                SELECT role, content, parts, id FROM messages \
                WHERE conversation_id = ? \
                ORDER BY id DESC \
                LIMIT ?\
             ) ORDER BY id ASC",
        )
        .bind(conversation_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let messages = rows
            .into_iter()
            .map(|(role_str, content, parts_json)| {
                let parts: Vec<MessagePart> = serde_json::from_str(&parts_json).unwrap_or_default();
                Message {
                    role: parse_role(&role_str),
                    content,
                    parts,
                }
            })
            .collect();
        Ok(messages)
    }

    /// Return the ID of the most recent conversation, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn latest_conversation_id(&self) -> Result<Option<i64>, MemoryError> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM conversations ORDER BY id DESC LIMIT 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| r.0))
    }

    /// Fetch a single message by its ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn message_by_id(&self, message_id: i64) -> Result<Option<Message>, MemoryError> {
        let row: Option<(String, String, String)> =
            sqlx::query_as("SELECT role, content, parts FROM messages WHERE id = ?")
                .bind(message_id)
                .fetch_optional(&self.pool)
                .await?;

        Ok(row.map(|(role_str, content, parts_json)| {
            let parts: Vec<MessagePart> = serde_json::from_str(&parts_json).unwrap_or_default();
            Message {
                role: parse_role(&role_str),
                content,
                parts,
            }
        }))
    }

    /// Fetch messages by a list of IDs in a single query.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn messages_by_ids(&self, ids: &[i64]) -> Result<Vec<(i64, Message)>, MemoryError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");

        let query =
            format!("SELECT id, role, content, parts FROM messages WHERE id IN ({placeholders})");
        let mut q = sqlx::query_as::<_, (i64, String, String, String)>(&query);
        for &id in ids {
            q = q.bind(id);
        }

        let rows = q.fetch_all(&self.pool).await?;

        Ok(rows
            .into_iter()
            .map(|(id, role_str, content, parts_json)| {
                let parts: Vec<MessagePart> = serde_json::from_str(&parts_json).unwrap_or_default();
                (
                    id,
                    Message {
                        role: parse_role(&role_str),
                        content,
                        parts,
                    },
                )
            })
            .collect())
    }

    /// Return message IDs and content for messages without embeddings.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn unembedded_message_ids(
        &self,
        limit: Option<usize>,
    ) -> Result<Vec<(i64, i64, String, String)>, MemoryError> {
        let effective_limit = limit.map_or(i64::MAX, |l| i64::try_from(l).unwrap_or(i64::MAX));

        let rows: Vec<(i64, i64, String, String)> = sqlx::query_as(
            "SELECT m.id, m.conversation_id, m.role, m.content \
             FROM messages m \
             LEFT JOIN embeddings_metadata em ON m.id = em.message_id \
             WHERE em.id IS NULL \
             ORDER BY m.id ASC \
             LIMIT ?",
        )
        .bind(effective_limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Count the number of messages in a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_messages(&self, conversation_id: i64) -> Result<i64, MemoryError> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
            .bind(conversation_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    /// Count messages in a conversation with id greater than `after_id`.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_messages_after(
        &self,
        conversation_id: i64,
        after_id: i64,
    ) -> Result<i64, MemoryError> {
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM messages WHERE conversation_id = ? AND id > ?")
                .bind(conversation_id)
                .bind(after_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0)
    }

    /// Load a range of messages after a given message ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_messages_range(
        &self,
        conversation_id: i64,
        after_message_id: i64,
        limit: usize,
    ) -> Result<Vec<(i64, String, String)>, MemoryError> {
        let effective_limit = i64::try_from(limit).unwrap_or(i64::MAX);

        let rows: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT id, role, content FROM messages \
             WHERE conversation_id = ? AND id > ? \
             ORDER BY id ASC LIMIT ?",
        )
        .bind(conversation_id)
        .bind(after_message_id)
        .bind(effective_limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> SqliteStore {
        SqliteStore::new(":memory:").await.unwrap()
    }

    #[tokio::test]
    async fn create_conversation_returns_id() {
        let store = test_store().await;
        let id1 = store.create_conversation().await.unwrap();
        let id2 = store.create_conversation().await.unwrap();
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[tokio::test]
    async fn save_and_load_messages() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "hello").await.unwrap();
        let msg_id2 = store
            .save_message(cid, "assistant", "hi there")
            .await
            .unwrap();

        assert_eq!(msg_id1, 1);
        assert_eq!(msg_id2, 2);

        let history = store.load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[0].content, "hello");
        assert_eq!(history[1].role, Role::Assistant);
        assert_eq!(history[1].content, "hi there");
    }

    #[tokio::test]
    async fn load_history_respects_limit() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        for i in 0..10 {
            store
                .save_message(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        let history = store.load_history(cid, 3).await.unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].content, "msg 7");
        assert_eq!(history[1].content, "msg 8");
        assert_eq!(history[2].content, "msg 9");
    }

    #[tokio::test]
    async fn latest_conversation_id_empty() {
        let store = test_store().await;
        assert!(store.latest_conversation_id().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn latest_conversation_id_returns_newest() {
        let store = test_store().await;
        store.create_conversation().await.unwrap();
        let id2 = store.create_conversation().await.unwrap();
        assert_eq!(store.latest_conversation_id().await.unwrap(), Some(id2));
    }

    #[tokio::test]
    async fn messages_isolated_per_conversation() {
        let store = test_store().await;
        let cid1 = store.create_conversation().await.unwrap();
        let cid2 = store.create_conversation().await.unwrap();

        store.save_message(cid1, "user", "conv1").await.unwrap();
        store.save_message(cid2, "user", "conv2").await.unwrap();

        let h1 = store.load_history(cid1, 50).await.unwrap();
        let h2 = store.load_history(cid2, 50).await.unwrap();
        assert_eq!(h1.len(), 1);
        assert_eq!(h1[0].content, "conv1");
        assert_eq!(h2.len(), 1);
        assert_eq!(h2[0].content, "conv2");
    }

    #[tokio::test]
    async fn pool_accessor_returns_valid_pool() {
        let store = test_store().await;
        let pool = store.pool();
        let row: (i64,) = sqlx::query_as("SELECT 1").fetch_one(pool).await.unwrap();
        assert_eq!(row.0, 1);
    }

    #[tokio::test]
    async fn embeddings_metadata_table_exists() {
        let store = test_store().await;
        let result: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='embeddings_metadata'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(result.0, 1);
    }

    #[tokio::test]
    async fn cascade_delete_removes_embeddings_metadata() {
        let store = test_store().await;
        let pool = store.pool();

        let cid = store.create_conversation().await.unwrap();
        let msg_id = store.save_message(cid, "user", "test").await.unwrap();

        let point_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO embeddings_metadata (message_id, qdrant_point_id, dimensions) \
             VALUES (?, ?, ?)",
        )
        .bind(msg_id)
        .bind(&point_id)
        .bind(768_i64)
        .execute(pool)
        .await
        .unwrap();

        let before: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM embeddings_metadata WHERE message_id = ?")
                .bind(msg_id)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(before.0, 1);

        sqlx::query("DELETE FROM messages WHERE id = ?")
            .bind(msg_id)
            .execute(pool)
            .await
            .unwrap();

        let after: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM embeddings_metadata WHERE message_id = ?")
                .bind(msg_id)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(after.0, 0);
    }

    #[tokio::test]
    async fn messages_by_ids_batch_fetch() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();
        let id1 = store.save_message(cid, "user", "hello").await.unwrap();
        let id2 = store.save_message(cid, "assistant", "hi").await.unwrap();
        let _id3 = store.save_message(cid, "user", "bye").await.unwrap();

        let results = store.messages_by_ids(&[id1, id2]).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, id1);
        assert_eq!(results[0].1.content, "hello");
        assert_eq!(results[1].0, id2);
        assert_eq!(results[1].1.content, "hi");
    }

    #[tokio::test]
    async fn messages_by_ids_empty_input() {
        let store = test_store().await;
        let results = store.messages_by_ids(&[]).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn messages_by_ids_nonexistent() {
        let store = test_store().await;
        let results = store.messages_by_ids(&[999, 1000]).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn message_by_id_fetches_existing() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();
        let msg_id = store.save_message(cid, "user", "hello").await.unwrap();

        let msg = store.message_by_id(msg_id).await.unwrap();
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, "hello");
    }

    #[tokio::test]
    async fn message_by_id_returns_none_for_nonexistent() {
        let store = test_store().await;
        let msg = store.message_by_id(999).await.unwrap();
        assert!(msg.is_none());
    }

    #[tokio::test]
    async fn unembedded_message_ids_returns_all_when_none_embedded() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        store.save_message(cid, "user", "msg1").await.unwrap();
        store.save_message(cid, "assistant", "msg2").await.unwrap();

        let unembedded = store.unembedded_message_ids(None).await.unwrap();
        assert_eq!(unembedded.len(), 2);
        assert_eq!(unembedded[0].3, "msg1");
        assert_eq!(unembedded[1].3, "msg2");
    }

    #[tokio::test]
    async fn unembedded_message_ids_excludes_embedded() {
        let store = test_store().await;
        let pool = store.pool();
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "msg1").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "msg2").await.unwrap();

        let point_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO embeddings_metadata (message_id, qdrant_point_id, dimensions) \
             VALUES (?, ?, ?)",
        )
        .bind(msg_id1)
        .bind(&point_id)
        .bind(768_i64)
        .execute(pool)
        .await
        .unwrap();

        let unembedded = store.unembedded_message_ids(None).await.unwrap();
        assert_eq!(unembedded.len(), 1);
        assert_eq!(unembedded[0].0, msg_id2);
        assert_eq!(unembedded[0].3, "msg2");
    }

    #[tokio::test]
    async fn unembedded_message_ids_respects_limit() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        for i in 0..10 {
            store
                .save_message(cid, "user", &format!("msg{i}"))
                .await
                .unwrap();
        }

        let unembedded = store.unembedded_message_ids(Some(3)).await.unwrap();
        assert_eq!(unembedded.len(), 3);
    }

    #[tokio::test]
    async fn count_messages_returns_correct_count() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        assert_eq!(store.count_messages(cid).await.unwrap(), 0);

        store.save_message(cid, "user", "msg1").await.unwrap();
        store.save_message(cid, "assistant", "msg2").await.unwrap();

        assert_eq!(store.count_messages(cid).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn count_messages_after_filters_correctly() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let id1 = store.save_message(cid, "user", "msg1").await.unwrap();
        let _id2 = store.save_message(cid, "assistant", "msg2").await.unwrap();
        let _id3 = store.save_message(cid, "user", "msg3").await.unwrap();

        assert_eq!(store.count_messages_after(cid, 0).await.unwrap(), 3);
        assert_eq!(store.count_messages_after(cid, id1).await.unwrap(), 2);
        assert_eq!(store.count_messages_after(cid, _id3).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn load_messages_range_basic() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "msg1").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "msg2").await.unwrap();
        let msg_id3 = store.save_message(cid, "user", "msg3").await.unwrap();

        let msgs = store.load_messages_range(cid, msg_id1, 10).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].0, msg_id2);
        assert_eq!(msgs[0].2, "msg2");
        assert_eq!(msgs[1].0, msg_id3);
        assert_eq!(msgs[1].2, "msg3");
    }

    #[tokio::test]
    async fn load_messages_range_respects_limit() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        store.save_message(cid, "user", "msg1").await.unwrap();
        store.save_message(cid, "assistant", "msg2").await.unwrap();
        store.save_message(cid, "user", "msg3").await.unwrap();

        let msgs = store.load_messages_range(cid, 0, 2).await.unwrap();
        assert_eq!(msgs.len(), 2);
    }
}
