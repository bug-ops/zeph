//! `Qdrant` collection + `SQLite` metadata for code chunks.

use qdrant_client::Qdrant;
use qdrant_client::qdrant::{
    CreateCollectionBuilder, CreateFieldIndexCollectionBuilder, DeletePointsBuilder, Distance,
    FieldType, Filter, PointStruct, PointsIdsList, ScalarQuantizationBuilder, ScoredPoint,
    SearchPointsBuilder, UpsertPointsBuilder, VectorParamsBuilder,
};

const CODE_COLLECTION: &str = "zeph_code_chunks";

/// `Qdrant` + `SQLite` dual-write store for code chunks.
pub struct CodeStore {
    qdrant: Qdrant,
    collection: String,
    pool: sqlx::SqlitePool,
}

/// Parameters for inserting a code chunk.
pub struct ChunkInsert<'a> {
    pub file_path: &'a str,
    pub language: &'a str,
    pub node_type: &'a str,
    pub entity_name: Option<&'a str>,
    pub line_start: usize,
    pub line_end: usize,
    pub code: &'a str,
    pub scope_chain: &'a str,
    pub content_hash: &'a str,
}

/// A search result from `Qdrant` with decoded payload.
#[derive(Debug)]
pub struct SearchHit {
    pub code: String,
    pub file_path: String,
    pub line_range: (usize, usize),
    pub score: f32,
    pub node_type: String,
    pub entity_name: Option<String>,
    pub scope_chain: String,
}

impl CodeStore {
    /// # Errors
    ///
    /// Returns an error if the `Qdrant` client fails to connect.
    pub fn new(qdrant_url: &str, pool: sqlx::SqlitePool) -> anyhow::Result<Self> {
        let qdrant = Qdrant::from_url(qdrant_url).build()?;
        Ok(Self {
            qdrant,
            collection: CODE_COLLECTION.into(),
            pool,
        })
    }

    /// Run `SQLite` migrations for chunk metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if migration execution fails.
    pub async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::migrate!().run(&self.pool).await?;
        Ok(())
    }

    /// Create collection with INT8 scalar quantization if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if `Qdrant` operations fail.
    pub async fn ensure_collection(&self, vector_size: u64) -> anyhow::Result<()> {
        if self.qdrant.collection_exists(&self.collection).await? {
            return Ok(());
        }

        self.qdrant
            .create_collection(
                CreateCollectionBuilder::new(&self.collection)
                    .vectors_config(VectorParamsBuilder::new(vector_size, Distance::Cosine))
                    .quantization_config(ScalarQuantizationBuilder::default()),
            )
            .await?;

        self.qdrant
            .create_field_index(CreateFieldIndexCollectionBuilder::new(
                &self.collection,
                "language",
                FieldType::Keyword,
            ))
            .await?;
        self.qdrant
            .create_field_index(CreateFieldIndexCollectionBuilder::new(
                &self.collection,
                "file_path",
                FieldType::Keyword,
            ))
            .await?;
        self.qdrant
            .create_field_index(CreateFieldIndexCollectionBuilder::new(
                &self.collection,
                "node_type",
                FieldType::Keyword,
            ))
            .await?;

        Ok(())
    }

    /// Upsert a code chunk into both `Qdrant` and `SQLite`.
    ///
    /// # Errors
    ///
    /// Returns an error if `Qdrant` or `SQLite` operations fail.
    pub async fn upsert_chunk(
        &self,
        chunk: &ChunkInsert<'_>,
        vector: Vec<f32>,
    ) -> anyhow::Result<String> {
        let point_id = uuid::Uuid::new_v4().to_string();

        let payload: std::collections::HashMap<String, qdrant_client::qdrant::Value> =
            serde_json::from_value(serde_json::json!({
                "file_path": chunk.file_path,
                "language": chunk.language,
                "node_type": chunk.node_type,
                "entity_name": chunk.entity_name,
                "line_start": chunk.line_start,
                "line_end": chunk.line_end,
                "code": chunk.code,
                "scope_chain": chunk.scope_chain,
                "content_hash": chunk.content_hash,
            }))?;

        self.qdrant
            .upsert_points(UpsertPointsBuilder::new(
                &self.collection,
                vec![PointStruct::new(point_id.clone(), vector, payload)],
            ))
            .await?;

        let line_start = i64::try_from(chunk.line_start)?;
        let line_end = i64::try_from(chunk.line_end)?;

        sqlx::query(
            "INSERT OR REPLACE INTO chunk_metadata \
             (qdrant_id, file_path, content_hash, line_start, line_end, language, node_type, entity_name) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&point_id)
        .bind(chunk.file_path)
        .bind(chunk.content_hash)
        .bind(line_start)
        .bind(line_end)
        .bind(chunk.language)
        .bind(chunk.node_type)
        .bind(chunk.entity_name)
        .execute(&self.pool)
        .await?;

        Ok(point_id)
    }

    /// Check if a chunk with this content hash already exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` query fails.
    pub async fn chunk_exists(&self, content_hash: &str) -> anyhow::Result<bool> {
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM chunk_metadata WHERE content_hash = ?")
                .bind(content_hash)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0 > 0)
    }

    /// Remove all chunks for a given file path from both stores.
    ///
    /// # Errors
    ///
    /// Returns an error if `Qdrant` or `SQLite` operations fail.
    pub async fn remove_file_chunks(&self, file_path: &str) -> anyhow::Result<usize> {
        let ids: Vec<(String,)> =
            sqlx::query_as("SELECT qdrant_id FROM chunk_metadata WHERE file_path = ?")
                .bind(file_path)
                .fetch_all(&self.pool)
                .await?;

        if ids.is_empty() {
            return Ok(0);
        }

        let point_ids = ids
            .iter()
            .map(|(id,)| id.clone().into())
            .collect::<Vec<_>>();

        self.qdrant
            .delete_points(
                DeletePointsBuilder::new(&self.collection).points(PointsIdsList { ids: point_ids }),
            )
            .await?;

        let count = ids.len();
        sqlx::query("DELETE FROM chunk_metadata WHERE file_path = ?")
            .bind(file_path)
            .execute(&self.pool)
            .await?;

        Ok(count)
    }

    /// Search for similar code chunks.
    ///
    /// # Errors
    ///
    /// Returns an error if `Qdrant` search fails.
    pub async fn search(
        &self,
        query_vector: Vec<f32>,
        limit: usize,
        filter: Option<Filter>,
    ) -> anyhow::Result<Vec<SearchHit>> {
        let mut builder = SearchPointsBuilder::new(&self.collection, query_vector, limit as u64)
            .with_payload(true);

        if let Some(f) = filter {
            builder = builder.filter(f);
        }

        let results = self.qdrant.search_points(builder).await?;

        Ok(results
            .result
            .iter()
            .filter_map(SearchHit::from_scored_point)
            .collect())
    }

    /// List all indexed file paths.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` query fails.
    pub async fn indexed_files(&self) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT DISTINCT file_path FROM chunk_metadata")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|(p,)| p).collect())
    }
}

impl SearchHit {
    fn from_scored_point(point: &ScoredPoint) -> Option<Self> {
        let p = &point.payload;
        let get_str = |key: &str| {
            p.get(key)
                .and_then(qdrant_client::qdrant::Value::as_str)
                .cloned()
        };
        let get_int = |key: &str| {
            p.get(key)
                .and_then(qdrant_client::qdrant::Value::as_integer)
                .and_then(|v| usize::try_from(v).ok())
        };

        Some(Self {
            code: get_str("code")?,
            file_path: get_str("file_path")?,
            line_range: (get_int("line_start")?, get_int("line_end")?),
            score: point.score,
            node_type: get_str("node_type")?,
            entity_name: get_str("entity_name"),
            scope_chain: get_str("scope_chain").unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    async fn setup_pool() -> sqlx::SqlitePool {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn chunk_exists_returns_false_then_true() {
        let pool = setup_pool().await;

        let exists = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM chunk_metadata WHERE content_hash = ?",
        )
        .bind("abc123")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(exists.0, 0);

        sqlx::query(
            "INSERT INTO chunk_metadata \
             (qdrant_id, file_path, content_hash, line_start, line_end, language, node_type) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("q1")
        .bind("src/main.rs")
        .bind("abc123")
        .bind(1_i64)
        .bind(10_i64)
        .bind("rust")
        .bind("function_item")
        .execute(&pool)
        .await
        .unwrap();

        let exists = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM chunk_metadata WHERE content_hash = ?",
        )
        .bind("abc123")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(exists.0 > 0);
    }

    #[tokio::test]
    async fn remove_file_chunks_cleans_sqlite() {
        let pool = setup_pool().await;

        for i in 0..3 {
            sqlx::query(
                "INSERT INTO chunk_metadata \
                 (qdrant_id, file_path, content_hash, line_start, line_end, language, node_type) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(format!("q{i}"))
            .bind("src/lib.rs")
            .bind(format!("hash{i}"))
            .bind(1_i64)
            .bind(10_i64)
            .bind("rust")
            .bind("function_item")
            .execute(&pool)
            .await
            .unwrap();
        }

        let ids: Vec<(String,)> =
            sqlx::query_as("SELECT qdrant_id FROM chunk_metadata WHERE file_path = ?")
                .bind("src/lib.rs")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(ids.len(), 3);

        sqlx::query("DELETE FROM chunk_metadata WHERE file_path = ?")
            .bind("src/lib.rs")
            .execute(&pool)
            .await
            .unwrap();

        let remaining: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM chunk_metadata WHERE file_path = ?")
                .bind("src/lib.rs")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(remaining.0, 0);
    }

    #[tokio::test]
    async fn indexed_files_distinct() {
        let pool = setup_pool().await;

        for (i, path) in ["src/a.rs", "src/b.rs", "src/a.rs"].iter().enumerate() {
            sqlx::query(
                "INSERT OR REPLACE INTO chunk_metadata \
                 (qdrant_id, file_path, content_hash, line_start, line_end, language, node_type) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(format!("q{i}"))
            .bind(path)
            .bind(format!("hash{i}"))
            .bind(1_i64)
            .bind(10_i64)
            .bind("rust")
            .bind("function_item")
            .execute(&pool)
            .await
            .unwrap();
        }

        let rows: Vec<(String,)> = sqlx::query_as("SELECT DISTINCT file_path FROM chunk_metadata")
            .fetch_all(&pool)
            .await
            .unwrap();
        let files: Vec<String> = rows.into_iter().map(|(p,)| p).collect();
        assert_eq!(files.len(), 2);
        assert!(files.contains(&"src/a.rs".to_string()));
        assert!(files.contains(&"src/b.rs".to_string()));
    }
}
