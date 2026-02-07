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
}
