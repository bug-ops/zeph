use anyhow::Context;
use qdrant_client::Qdrant;
use qdrant_client::qdrant::{
    Condition, CreateCollectionBuilder, Distance, Filter, PointStruct, SearchPointsBuilder,
    UpsertPointsBuilder, VectorParamsBuilder,
};
use sqlx::SqlitePool;

const COLLECTION_NAME: &str = "zeph_conversations";

pub struct QdrantStore {
    client: Qdrant,
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
    pub conversation_id: Option<i64>,
    pub role: Option<String>,
}

#[derive(Debug)]
pub struct SearchResult {
    pub message_id: i64,
    pub conversation_id: i64,
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
    pub fn new(url: &str, pool: SqlitePool) -> anyhow::Result<Self> {
        let client = Qdrant::from_url(url)
            .build()
            .context("failed to create Qdrant client")?;

        Ok(Self {
            client,
            collection: COLLECTION_NAME.into(),
            pool,
        })
    }

    /// Ensure the collection exists in Qdrant with the given vector size.
    ///
    /// Idempotent: no-op if the collection already exists.
    ///
    /// # Errors
    ///
    /// Returns an error if Qdrant cannot be reached or collection creation fails.
    pub async fn ensure_collection(&self, vector_size: u64) -> anyhow::Result<()> {
        if self.client.collection_exists(&self.collection).await? {
            return Ok(());
        }

        self.client
            .create_collection(
                CreateCollectionBuilder::new(&self.collection)
                    .vectors_config(VectorParamsBuilder::new(vector_size, Distance::Cosine)),
            )
            .await
            .context("failed to create Qdrant collection")?;

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
        message_id: i64,
        conversation_id: i64,
        role: &str,
        vector: Vec<f32>,
        is_summary: bool,
        model: &str,
    ) -> anyhow::Result<String> {
        let point_id = uuid::Uuid::new_v4().to_string();
        let dimensions = i64::try_from(vector.len()).context("vector length exceeds i64")?;

        let payload: serde_json::Value = serde_json::json!({
            "message_id": message_id,
            "conversation_id": conversation_id,
            "role": role,
            "is_summary": is_summary,
        });
        let payload_map: std::collections::HashMap<String, qdrant_client::qdrant::Value> =
            serde_json::from_value(payload).context("failed to convert payload")?;

        let point = PointStruct::new(point_id.clone(), vector, payload_map);

        self.client
            .upsert_points(UpsertPointsBuilder::new(&self.collection, vec![point]))
            .await
            .context("failed to upsert point to Qdrant")?;

        sqlx::query(
            "INSERT INTO embeddings_metadata (message_id, qdrant_point_id, dimensions, model) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(message_id)
        .bind(&point_id)
        .bind(dimensions)
        .bind(model)
        .execute(&self.pool)
        .await
        .context("failed to insert embeddings metadata")?;

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
    ) -> anyhow::Result<Vec<SearchResult>> {
        let limit_u64 = u64::try_from(limit).context("limit exceeds u64")?;

        let mut builder =
            SearchPointsBuilder::new(&self.collection, query_vector.to_vec(), limit_u64)
                .with_payload(true);

        if let Some(ref f) = filter {
            let mut conditions = Vec::new();

            if let Some(cid) = f.conversation_id {
                conditions.push(Condition::matches("conversation_id", cid));
            }
            if let Some(ref role) = f.role {
                conditions.push(Condition::matches("role", role.clone()));
            }

            if !conditions.is_empty() {
                builder = builder.filter(Filter::must(conditions));
            }
        }

        let results = self
            .client
            .search_points(builder)
            .await
            .context("failed to search Qdrant")?;

        let search_results = results
            .result
            .into_iter()
            .filter_map(|point| {
                let payload = &point.payload;

                let message_id = payload.get("message_id")?.as_integer()?;
                let conversation_id = payload.get("conversation_id")?.as_integer()?;

                Some(SearchResult {
                    message_id,
                    conversation_id,
                    score: point.score,
                })
            })
            .collect();

        Ok(search_results)
    }

    /// Check whether an embedding already exists for the given message ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` query fails.
    pub async fn has_embedding(&self, message_id: i64) -> anyhow::Result<bool> {
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM embeddings_metadata WHERE message_id = ?")
                .bind(message_id)
                .fetch_one(&self.pool)
                .await
                .context("failed to check embeddings metadata")?;

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
