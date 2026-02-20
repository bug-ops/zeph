pub use qdrant_client::qdrant::Filter;
use sqlx::SqlitePool;

use crate::error::MemoryError;
use crate::qdrant_ops::QdrantOps;
use crate::types::{ConversationId, MessageId};
use crate::vector_store::{FieldCondition, FieldValue, VectorFilter, VectorPoint, VectorStore};

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

pub struct EmbeddingStore {
    ops: Box<dyn VectorStore>,
    collection: String,
    pool: SqlitePool,
}

impl std::fmt::Debug for EmbeddingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingStore")
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

impl EmbeddingStore {
    /// Create a new `EmbeddingStore` connected to the given Qdrant URL.
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
            ops: Box::new(ops),
            collection: COLLECTION_NAME.into(),
            pool,
        })
    }

    #[must_use]
    pub fn with_store(store: Box<dyn VectorStore>, pool: SqlitePool) -> Self {
        Self {
            ops: store,
            collection: COLLECTION_NAME.into(),
            pool,
        }
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

        let payload = std::collections::HashMap::from([
            ("message_id".to_owned(), serde_json::json!(message_id.0)),
            (
                "conversation_id".to_owned(),
                serde_json::json!(conversation_id.0),
            ),
            ("role".to_owned(), serde_json::json!(role)),
            (
                "is_summary".to_owned(),
                serde_json::json!(kind.is_summary()),
            ),
        ]);

        let point = VectorPoint {
            id: point_id.clone(),
            vector,
            payload,
        };

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

        let vector_filter = filter.as_ref().and_then(|f| {
            let mut must = Vec::new();
            if let Some(cid) = f.conversation_id {
                must.push(FieldCondition {
                    field: "conversation_id".into(),
                    value: FieldValue::Integer(cid.0),
                });
            }
            if let Some(ref role) = f.role {
                must.push(FieldCondition {
                    field: "role".into(),
                    value: FieldValue::Text(role.clone()),
                });
            }
            if must.is_empty() {
                None
            } else {
                Some(VectorFilter {
                    must,
                    must_not: vec![],
                })
            }
        });

        let results = self
            .ops
            .search(
                &self.collection,
                query_vector.to_vec(),
                limit_u64,
                vector_filter,
            )
            .await?;

        let search_results = results
            .into_iter()
            .filter_map(|point| {
                let message_id = MessageId(point.payload.get("message_id")?.as_i64()?);
                let conversation_id =
                    ConversationId(point.payload.get("conversation_id")?.as_i64()?);
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
        let payload_map: std::collections::HashMap<String, serde_json::Value> =
            serde_json::from_value(payload)?;
        let point = VectorPoint {
            id: point_id.clone(),
            vector,
            payload: payload_map,
        };
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
        filter: Option<VectorFilter>,
    ) -> Result<Vec<crate::ScoredVectorPoint>, MemoryError> {
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
    use crate::in_memory_store::InMemoryVectorStore;
    use crate::sqlite::SqliteStore;

    async fn setup() -> (SqliteStore, SqlitePool) {
        let store = SqliteStore::new(":memory:").await.unwrap();
        let pool = store.pool().clone();
        (store, pool)
    }

    async fn setup_with_store() -> (EmbeddingStore, SqliteStore) {
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite.pool().clone();
        let mem_store = Box::new(InMemoryVectorStore::new());
        let embedding_store = EmbeddingStore::with_store(mem_store, pool);
        // Create collection first
        embedding_store.ensure_collection(4).await.unwrap();
        (embedding_store, sqlite)
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
    async fn embedding_store_search_empty_returns_empty() {
        let (store, _sqlite) = setup_with_store().await;
        let results = store.search(&[1.0, 0.0, 0.0, 0.0], 10, None).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn embedding_store_store_and_search() {
        let (store, sqlite) = setup_with_store().await;
        let cid = sqlite.create_conversation().await.unwrap();
        let msg_id = sqlite
            .save_message(cid, "user", "test message")
            .await
            .unwrap();

        store
            .store(
                msg_id,
                cid,
                "user",
                vec![1.0, 0.0, 0.0, 0.0],
                MessageKind::Regular,
                "test-model",
            )
            .await
            .unwrap();

        let results = store.search(&[1.0, 0.0, 0.0, 0.0], 5, None).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_id, msg_id);
        assert_eq!(results[0].conversation_id, cid);
        assert!((results[0].score - 1.0).abs() < 0.001);
    }

    #[tokio::test]
    async fn embedding_store_has_embedding_false_for_unknown() {
        let (store, sqlite) = setup_with_store().await;
        let cid = sqlite.create_conversation().await.unwrap();
        let msg_id = sqlite.save_message(cid, "user", "test").await.unwrap();
        assert!(!store.has_embedding(msg_id).await.unwrap());
    }

    #[tokio::test]
    async fn embedding_store_has_embedding_true_after_store() {
        let (store, sqlite) = setup_with_store().await;
        let cid = sqlite.create_conversation().await.unwrap();
        let msg_id = sqlite.save_message(cid, "user", "hello").await.unwrap();

        store
            .store(
                msg_id,
                cid,
                "user",
                vec![0.0, 1.0, 0.0, 0.0],
                MessageKind::Regular,
                "test-model",
            )
            .await
            .unwrap();

        assert!(store.has_embedding(msg_id).await.unwrap());
    }

    #[tokio::test]
    async fn embedding_store_search_with_conversation_filter() {
        let (store, sqlite) = setup_with_store().await;
        let cid1 = sqlite.create_conversation().await.unwrap();
        let cid2 = sqlite.create_conversation().await.unwrap();
        let msg1 = sqlite.save_message(cid1, "user", "msg1").await.unwrap();
        let msg2 = sqlite.save_message(cid2, "user", "msg2").await.unwrap();

        store
            .store(
                msg1,
                cid1,
                "user",
                vec![1.0, 0.0, 0.0, 0.0],
                MessageKind::Regular,
                "m",
            )
            .await
            .unwrap();
        store
            .store(
                msg2,
                cid2,
                "user",
                vec![1.0, 0.0, 0.0, 0.0],
                MessageKind::Regular,
                "m",
            )
            .await
            .unwrap();

        let results = store
            .search(
                &[1.0, 0.0, 0.0, 0.0],
                10,
                Some(SearchFilter {
                    conversation_id: Some(cid1),
                    role: None,
                }),
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].conversation_id, cid1);
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
