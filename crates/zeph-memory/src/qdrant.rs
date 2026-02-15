pub use qdrant_client::qdrant::Filter;
use qdrant_client::qdrant::{Condition, PointStruct};
use sqlx::SqlitePool;

use crate::error::MemoryError;
use crate::qdrant_ops::QdrantOps;
use crate::types::{ConversationId, MessageId};

/// Distinguishes regular messages from summaries when storing embeddings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Regular,
    Summary,
}

impl MessageKind {
    #[must_use]
    pub fn is_summary(self) -> bool {
        matches!(self, Self::Summary)
    }
}

const COLLECTION_NAME: &str = "zeph_conversations";

/// Ensure a Qdrant collection exists with cosine distance vectors.
///
/// Idempotent: no-op if the collection already exists.
///
/// # Errors
///
/// Returns an error if Qdrant cannot be reached or collection creation fails.
pub async fn ensure_qdrant_collection(
    ops: &QdrantOps,
    collection: &str,
    vector_size: u64,
) -> Result<(), Box<qdrant_client::QdrantError>> {
    ops.ensure_collection(collection, vector_size).await
}

pub struct QdrantStore {
    ops: QdrantOps,
    collection: String,
    pool: SqlitePool,
}

impl std::fmt::Debug for QdrantStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QdrantStore")
            .field("collection", &self.collection)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct SearchFilter {
    pub conversation_id: Option<ConversationId>,
    pub role: Option<String>,
}

#[derive(Debug)]
pub struct SearchResult {
    pub message_id: MessageId,
    pub conversation_id: ConversationId,
    pub score: f32,
}

impl QdrantStore {
    /// Create a new `QdrantStore` connected to the given Qdrant URL.
    ///
    /// The `pool` is used for `SQLite` metadata operations on the `embeddings_metadata`
    /// table (which must already exist via sqlx migrations).
    ///
    /// # Errors
    ///
    /// Returns an error if the Qdrant client cannot be created.
    pub fn new(url: &str, pool: SqlitePool) -> Result<Self, MemoryError> {
        let ops = QdrantOps::new(url).map_err(MemoryError::Qdrant)?;

        Ok(Self {
            ops,
            collection: COLLECTION_NAME.into(),
            pool,
        })
    }

    /// Access the underlying `QdrantOps`.
    #[must_use]
    pub fn ops(&self) -> &QdrantOps {
        &self.ops
    }

    /// Ensure the collection exists in Qdrant with the given vector size.
    ///
    /// Idempotent: no-op if the collection already exists.
    ///
    /// # Errors
    ///
    /// Returns an error if Qdrant cannot be reached or collection creation fails.
    pub async fn ensure_collection(&self, vector_size: u64) -> Result<(), MemoryError> {
        self.ops
            .ensure_collection(&self.collection, vector_size)
            .await?;
        Ok(())
    }

    /// Store a vector in Qdrant and persist metadata to `SQLite`.
    ///
    /// Returns the UUID of the newly created Qdrant point.
    ///
    /// # Errors
    ///
    /// Returns an error if the Qdrant upsert or `SQLite` insert fails.
    pub async fn store(
        &self,
        message_id: MessageId,
        conversation_id: ConversationId,
        role: &str,
        vector: Vec<f32>,
        kind: MessageKind,
        model: &str,
    ) -> Result<String, MemoryError> {
        let point_id = uuid::Uuid::new_v4().to_string();
        let dimensions = i64::try_from(vector.len())?;

        let payload = serde_json::json!({
            "message_id": message_id.0,
            "conversation_id": conversation_id.0,
            "role": role,
            "is_summary": kind.is_summary(),
        });
        let payload_map = QdrantOps::json_to_payload(payload)?;

        let point = PointStruct::new(point_id.clone(), vector, payload_map);

        self.ops.upsert(&self.collection, vec![point]).await?;

        sqlx::query(
            "INSERT INTO embeddings_metadata (message_id, qdrant_point_id, dimensions, model) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(message_id, model) DO UPDATE SET \
             qdrant_point_id = excluded.qdrant_point_id, dimensions = excluded.dimensions",
        )
        .bind(message_id)
        .bind(&point_id)
        .bind(dimensions)
        .bind(model)
        .execute(&self.pool)
        .await?;

        Ok(point_id)
    }

    /// Search for similar vectors in Qdrant, returning up to `limit` results.
    ///
    /// # Errors
    ///
    /// Returns an error if the Qdrant search fails.
    pub async fn search(
        &self,
        query_vector: &[f32],
        limit: usize,
        filter: Option<SearchFilter>,
    ) -> Result<Vec<SearchResult>, MemoryError> {
        let limit_u64 = u64::try_from(limit)?;

        let qdrant_filter = filter.as_ref().and_then(|f| {
            let mut conditions = Vec::new();
            if let Some(cid) = f.conversation_id {
                conditions.push(Condition::matches("conversation_id", cid.0));
            }
            if let Some(ref role) = f.role {
                conditions.push(Condition::matches("role", role.clone()));
            }
            if conditions.is_empty() {
                None
            } else {
                Some(Filter::must(conditions))
            }
        });

        let results = self
            .ops
            .search(
                &self.collection,
                query_vector.to_vec(),
                limit_u64,
                qdrant_filter,
            )
            .await?;

        let search_results = results
            .into_iter()
            .filter_map(|point| {
                let payload = &point.payload;
                let message_id = MessageId(payload.get("message_id")?.as_integer()?);
                let conversation_id = ConversationId(payload.get("conversation_id")?.as_integer()?);
                Some(SearchResult {
                    message_id,
                    conversation_id,
                    score: point.score,
                })
            })
            .collect();

        Ok(search_results)
    }

    /// Ensure a named collection exists in Qdrant with the given vector size.
    ///
    /// # Errors
    ///
    /// Returns an error if Qdrant cannot be reached or collection creation fails.
    pub async fn ensure_named_collection(
        &self,
        name: &str,
        vector_size: u64,
    ) -> Result<(), MemoryError> {
        self.ops.ensure_collection(name, vector_size).await?;
        Ok(())
    }

    /// Store a vector in a named Qdrant collection with arbitrary payload.
    ///
    /// Returns the UUID of the newly created point.
    ///
    /// # Errors
    ///
    /// Returns an error if the Qdrant upsert fails.
    pub async fn store_to_collection(
        &self,
        collection: &str,
        payload: serde_json::Value,
        vector: Vec<f32>,
    ) -> Result<String, MemoryError> {
        let point_id = uuid::Uuid::new_v4().to_string();
        let payload_map = QdrantOps::json_to_payload(payload)?;
        let point = PointStruct::new(point_id.clone(), vector, payload_map);
        self.ops.upsert(collection, vec![point]).await?;
        Ok(point_id)
    }

    /// Search a named Qdrant collection, returning scored points with payloads.
    ///
    /// # Errors
    ///
    /// Returns an error if the Qdrant search fails.
    pub async fn search_collection(
        &self,
        collection: &str,
        query_vector: &[f32],
        limit: usize,
        filter: Option<Filter>,
    ) -> Result<Vec<qdrant_client::qdrant::ScoredPoint>, MemoryError> {
        let limit_u64 = u64::try_from(limit)?;
        let results = self
            .ops
            .search(collection, query_vector.to_vec(), limit_u64, filter)
            .await?;
        Ok(results)
    }

    /// Check whether an embedding already exists for the given message ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` query fails.
    pub async fn has_embedding(&self, message_id: MessageId) -> Result<bool, MemoryError> {
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM embeddings_metadata WHERE message_id = ?")
                .bind(message_id)
                .fetch_one(&self.pool)
                .await?;

        Ok(row.0 > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::SqliteStore;

    async fn setup() -> (SqliteStore, SqlitePool) {
        let store = SqliteStore::new(":memory:").await.unwrap();
        let pool = store.pool().clone();
        (store, pool)
    }

    #[tokio::test]
    async fn has_embedding_returns_false_when_none() {
        let (_store, pool) = setup().await;

        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM embeddings_metadata WHERE message_id = ?")
                .bind(999_i64)
                .fetch_one(&pool)
                .await
                .unwrap();

        assert_eq!(row.0, 0);
    }

    #[tokio::test]
    async fn insert_and_query_embeddings_metadata() {
        let (sqlite, pool) = setup().await;
        let cid = sqlite.create_conversation().await.unwrap();
        let msg_id = sqlite.save_message(cid, "user", "test").await.unwrap();

        let point_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO embeddings_metadata (message_id, qdrant_point_id, dimensions, model) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(msg_id)
        .bind(&point_id)
        .bind(768_i64)
        .bind("qwen3-embedding")
        .execute(&pool)
        .await
        .unwrap();

        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM embeddings_metadata WHERE message_id = ?")
                .bind(msg_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, 1);
    }

    #[tokio::test]
    async fn unique_constraint_on_message_and_model() {
        let (sqlite, pool) = setup().await;
        let cid = sqlite.create_conversation().await.unwrap();
        let msg_id = sqlite.save_message(cid, "user", "test").await.unwrap();

        let point_id1 = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO embeddings_metadata (message_id, qdrant_point_id, dimensions, model) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(msg_id)
        .bind(&point_id1)
        .bind(768_i64)
        .bind("qwen3-embedding")
        .execute(&pool)
        .await
        .unwrap();

        let point_id2 = uuid::Uuid::new_v4().to_string();
        let result = sqlx::query(
            "INSERT INTO embeddings_metadata (message_id, qdrant_point_id, dimensions, model) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(msg_id)
        .bind(&point_id2)
        .bind(768_i64)
        .bind("qwen3-embedding")
        .execute(&pool)
        .await;

        assert!(result.is_err());
    }
}
