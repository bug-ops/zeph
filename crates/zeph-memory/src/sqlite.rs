use std::str::FromStr;

use anyhow::Context;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use zeph_llm::provider::{Message, Role};

#[derive(Debug)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (or create) the `SQLite` database and run migrations.
    ///
    /// Enables foreign key constraints at connection level so that
    /// `ON DELETE CASCADE` and other FK rules are enforced.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or migrations fail.
    pub async fn new(path: &str) -> anyhow::Result<Self> {
        let url = if path == ":memory:" {
            "sqlite::memory:".to_string()
        } else {
            format!("sqlite:{path}?mode=rwc")
        };

        let opts = SqliteConnectOptions::from_str(&url)?
            .create_if_missing(true)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .context("failed to open SQLite database")?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("failed to run migrations")?;

        Ok(Self { pool })
    }

    /// Expose the underlying pool for shared access by other stores.
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Create a new conversation and return its ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn create_conversation(&self) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as("INSERT INTO conversations DEFAULT VALUES RETURNING id")
            .fetch_one(&self.pool)
            .await
            .context("failed to create conversation")?;
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
    ) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO messages (conversation_id, role, content) VALUES (?, ?, ?) RETURNING id",
        )
        .bind(conversation_id)
        .bind(role)
        .bind(content)
        .fetch_one(&self.pool)
        .await
        .context("failed to save message")?;
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
    ) -> anyhow::Result<Vec<Message>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT role, content FROM messages \
             WHERE conversation_id = ? \
             ORDER BY id ASC \
             LIMIT ?",
        )
        .bind(conversation_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("failed to load history")?;

        let messages = rows
            .into_iter()
            .map(|(role_str, content)| Message {
                role: parse_role(&role_str),
                content,
            })
            .collect();
        Ok(messages)
    }

    /// Return the ID of the most recent conversation, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn latest_conversation_id(&self) -> anyhow::Result<Option<i64>> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM conversations ORDER BY id DESC LIMIT 1")
                .fetch_optional(&self.pool)
                .await
                .context("failed to fetch latest conversation")?;
        Ok(row.map(|r| r.0))
    }

    /// Fetch a single message by its ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn message_by_id(&self, message_id: i64) -> anyhow::Result<Option<Message>> {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT role, content FROM messages WHERE id = ?")
                .bind(message_id)
                .fetch_optional(&self.pool)
                .await
                .context("failed to fetch message by id")?;

        Ok(row.map(|(role_str, content)| Message {
            role: parse_role(&role_str),
            content,
        }))
    }

    /// Return message IDs and content for messages without embeddings.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn unembedded_message_ids(
        &self,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<(i64, i64, String, String)>> {
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
        .await
        .context("failed to fetch unembedded message ids")?;

        Ok(rows)
    }

    /// Count the number of messages in a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_messages(&self, conversation_id: i64) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
            .bind(conversation_id)
            .fetch_one(&self.pool)
            .await
            .context("failed to count messages")?;
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
    ) -> anyhow::Result<Vec<(i64, String, String)>> {
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
        .await
        .context("failed to load messages range")?;

        Ok(rows)
    }

    /// Save a summary and return its ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn save_summary(
        &self,
        conversation_id: i64,
        content: &str,
        first_message_id: i64,
        last_message_id: i64,
        token_estimate: i64,
    ) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO summaries (conversation_id, content, first_message_id, last_message_id, token_estimate) \
             VALUES (?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(conversation_id)
        .bind(content)
        .bind(first_message_id)
        .bind(last_message_id)
        .bind(token_estimate)
        .fetch_one(&self.pool)
        .await
        .context("failed to save summary")?;
        Ok(row.0)
    }

    /// Load all summaries for a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_summaries(
        &self,
        conversation_id: i64,
    ) -> anyhow::Result<Vec<(i64, i64, String, i64, i64, i64)>> {
        let rows: Vec<(i64, i64, String, i64, i64, i64)> = sqlx::query_as(
            "SELECT id, conversation_id, content, first_message_id, last_message_id, token_estimate \
             FROM summaries WHERE conversation_id = ? ORDER BY id ASC",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await
        .context("failed to load summaries")?;

        Ok(rows)
    }

    /// Get the last message ID covered by the most recent summary.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn latest_summary_last_message_id(
        &self,
        conversation_id: i64,
    ) -> anyhow::Result<Option<i64>> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT last_message_id FROM summaries \
             WHERE conversation_id = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await
        .context("failed to fetch latest summary")?;

        Ok(row.map(|r| r.0))
    }
}

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

    #[tokio::test]
    async fn save_and_load_summary() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "hello").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "hi").await.unwrap();

        let summary_id = store
            .save_summary(cid, "User greeted assistant", msg_id1, msg_id2, 5)
            .await
            .unwrap();

        let summaries = store.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].0, summary_id);
        assert_eq!(summaries[0].2, "User greeted assistant");
        assert_eq!(summaries[0].3, msg_id1);
        assert_eq!(summaries[0].4, msg_id2);
        assert_eq!(summaries[0].5, 5);
    }

    #[tokio::test]
    async fn load_summaries_empty() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let summaries = store.load_summaries(cid).await.unwrap();
        assert!(summaries.is_empty());
    }

    #[tokio::test]
    async fn load_summaries_ordered() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "m1").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "m2").await.unwrap();
        let msg_id3 = store.save_message(cid, "user", "m3").await.unwrap();

        let s1 = store
            .save_summary(cid, "summary1", msg_id1, msg_id2, 3)
            .await
            .unwrap();
        let s2 = store
            .save_summary(cid, "summary2", msg_id2, msg_id3, 3)
            .await
            .unwrap();

        let summaries = store.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].0, s1);
        assert_eq!(summaries[1].0, s2);
    }

    #[tokio::test]
    async fn latest_summary_last_message_id_none() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let last = store.latest_summary_last_message_id(cid).await.unwrap();
        assert!(last.is_none());
    }

    #[tokio::test]
    async fn latest_summary_last_message_id_some() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "m1").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "m2").await.unwrap();
        let msg_id3 = store.save_message(cid, "user", "m3").await.unwrap();

        store
            .save_summary(cid, "summary1", msg_id1, msg_id2, 3)
            .await
            .unwrap();
        store
            .save_summary(cid, "summary2", msg_id2, msg_id3, 3)
            .await
            .unwrap();

        let last = store.latest_summary_last_message_id(cid).await.unwrap();
        assert_eq!(last, Some(msg_id3));
    }

    #[tokio::test]
    async fn cascade_delete_removes_summaries() {
        let store = test_store().await;
        let pool = store.pool();
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "m1").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "m2").await.unwrap();

        store
            .save_summary(cid, "summary", msg_id1, msg_id2, 3)
            .await
            .unwrap();

        let before: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM summaries WHERE conversation_id = ?")
                .bind(cid)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(before.0, 1);

        sqlx::query("DELETE FROM conversations WHERE id = ?")
            .bind(cid)
            .execute(pool)
            .await
            .unwrap();

        let after: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM summaries WHERE conversation_id = ?")
                .bind(cid)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(after.0, 0);
    }
}
