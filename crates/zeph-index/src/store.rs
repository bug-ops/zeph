// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Qdrant collection + `SQLite` metadata for code chunks.
//!
//! [`CodeStore`] is a **dual-write store**: every chunk is simultaneously stored as
//! a vector point in Qdrant (for similarity search) and as a metadata row in `SQLite`
//! (for exact-hash deduplication and file-path bookkeeping).
//!
//! ## Why dual-write?
//!
//! Qdrant does not expose a cheap "does this hash exist?" query, so `SQLite` acts as a
//! fast lookup table. Before embedding a file the indexer fetches all known hashes for
//! that file from `SQLite` in a single `IN (…)` query; only chunks whose hash is absent
//! are sent to the LLM for embedding.
//!
//! ## Collection name
//!
//! The Qdrant collection is always named `"zeph_code_chunks"`. The `SQLite` table is
//! `chunk_metadata`, created by the `zeph-db` migration layer at startup.

#[allow(unused_imports)]
use zeph_db::sql;
use zeph_memory::{FieldCondition, FieldValue, QdrantOps, VectorFilter, VectorPoint, VectorStore};

use zeph_common::{EmbeddingVector, Normalized};

use crate::error::Result;

const CODE_COLLECTION: &str = "zeph_code_chunks";

/// Qdrant + `SQLite` dual-write store for code chunks.
///
/// `CodeStore` is the persistence layer for the indexing pipeline. It is cheaply
/// cloneable (all fields are reference-counted) and can safely be shared across async
/// tasks.
///
/// # Lifecycle
///
/// 1. Call [`CodeStore::with_ops`] to construct.
/// 2. Call [`CodeStore::ensure_collection`] once at startup to create the Qdrant
///    collection if it does not yet exist.
/// 3. Use [`CodeStore::upsert_chunks_batch`] during indexing and [`CodeStore::search`]
///    during retrieval.
#[derive(Clone)]
pub struct CodeStore {
    ops: QdrantOps,
    collection: String,
    pool: zeph_db::DbPool,
}

/// Borrowed parameters for inserting a single code chunk.
///
/// All string fields are borrowed to avoid cloning the source data during batch
/// construction. The struct is consumed by [`CodeStore::upsert_chunk`] and
/// [`CodeStore::upsert_chunks_batch`].
pub struct ChunkInsert<'a> {
    /// Relative path from the project root (e.g. `"src/lib.rs"`).
    pub file_path: &'a str,
    /// Language identifier (e.g. `"rust"`). See [`crate::languages::Lang::id`].
    pub language: &'a str,
    /// Tree-sitter node kind (e.g. `"function_item"`).
    pub node_type: &'a str,
    /// Optional symbol name extracted by the chunker.
    pub entity_name: Option<&'a str>,
    /// 1-based inclusive start line.
    pub line_start: usize,
    /// 1-based inclusive end line.
    pub line_end: usize,
    /// Raw source text of the chunk.
    pub code: &'a str,
    /// `">"` separated scope nesting path.
    pub scope_chain: &'a str,
    /// Blake3 hex digest of `code`.
    pub content_hash: &'a str,
}

/// A single search result returned by [`CodeStore::search`].
///
/// Decoded from the Qdrant vector point payload by `SearchHit::from_payload`.
/// Points whose payload is missing required fields are silently dropped.
#[derive(Debug)]
pub struct SearchHit {
    /// Raw source text of the matching chunk.
    pub code: String,
    /// Relative file path from the project root.
    pub file_path: String,
    /// 1-based inclusive `(start_line, end_line)` within the file.
    pub line_range: (usize, usize),
    /// Cosine similarity score returned by Qdrant (higher is more similar).
    pub score: f32,
    /// Tree-sitter node kind of the primary AST node.
    pub node_type: String,
    /// Symbol name, if available.
    pub entity_name: Option<String>,
    /// `">"` separated scope chain.
    pub scope_chain: String,
}

impl CodeStore {
    /// Create a `CodeStore` from a pre-built [`QdrantOps`] instance and a `SQLite` pool.
    ///
    /// The Qdrant collection is not created here — call [`CodeStore::ensure_collection`]
    /// before performing any upserts.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_index::store::CodeStore;
    /// use zeph_memory::QdrantOps;
    /// # async fn example() -> zeph_index::Result<()> {
    /// # let pool: zeph_db::DbPool = panic!("placeholder");
    ///
    /// let ops = QdrantOps::new("http://localhost:6334", None).unwrap();
    /// let store = CodeStore::with_ops(ops, pool);
    /// store.ensure_collection(1536).await?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn with_ops(ops: QdrantOps, pool: zeph_db::DbPool) -> Self {
        Self {
            ops,
            collection: CODE_COLLECTION.into(),
            pool,
        }
    }

    /// Create collection with INT8 scalar quantization if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if `Qdrant` operations fail.
    pub async fn ensure_collection(&self, vector_size: u64) -> Result<()> {
        self.ops
            .ensure_collection_with_quantization(
                &self.collection,
                vector_size,
                &["language", "file_path", "node_type"],
            )
            .await?;
        Ok(())
    }

    /// Upsert a code chunk into both `Qdrant` and `SQLite`.
    ///
    /// # Errors
    ///
    /// Returns an error if `Qdrant` or `SQLite` operations fail.
    pub async fn upsert_chunk(&self, chunk: &ChunkInsert<'_>, vector: Vec<f32>) -> Result<String> {
        let point_id = uuid::Uuid::new_v4().to_string();

        let payload = serde_json::json!({
            "file_path": chunk.file_path,
            "language": chunk.language,
            "node_type": chunk.node_type,
            "entity_name": chunk.entity_name,
            "line_start": chunk.line_start,
            "line_end": chunk.line_end,
            "code": chunk.code,
            "scope_chain": chunk.scope_chain,
            "content_hash": chunk.content_hash,
        });

        let payload_map = match payload {
            serde_json::Value::Object(m) => m.into_iter().collect(),
            _ => std::collections::HashMap::new(),
        };

        VectorStore::upsert(
            &self.ops,
            &self.collection,
            vec![VectorPoint {
                id: point_id.clone(),
                vector,
                payload: payload_map,
            }],
        )
        .await?;

        let line_start = i64::try_from(chunk.line_start)?;
        let line_end = i64::try_from(chunk.line_end)?;

        zeph_db::query(
            sql!("INSERT INTO chunk_metadata \
             (qdrant_id, file_path, content_hash, line_start, line_end, language, node_type, entity_name) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(file_path, content_hash) DO UPDATE SET \
               qdrant_id = excluded.qdrant_id, \
               line_start = excluded.line_start, line_end = excluded.line_end, \
               language = excluded.language, node_type = excluded.node_type, \
               entity_name = excluded.entity_name"),
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

    /// Upsert multiple chunks into both `Qdrant` and `SQLite` in a single batch.
    ///
    /// All vector points are sent to `Qdrant` in one request and all metadata rows are inserted
    /// in a single `SQLite` transaction, reducing per-chunk overhead during full-project indexing.
    ///
    /// # Errors
    ///
    /// Returns an error if `Qdrant` or `SQLite` operations fail.
    pub async fn upsert_chunks_batch(
        &self,
        chunks: Vec<(ChunkInsert<'_>, Vec<f32>)>,
    ) -> Result<Vec<String>> {
        if chunks.is_empty() {
            return Ok(Vec::new());
        }

        let mut point_ids: Vec<String> = Vec::with_capacity(chunks.len());
        let mut points: Vec<VectorPoint> = Vec::with_capacity(chunks.len());

        for (chunk, vector) in &chunks {
            let point_id = uuid::Uuid::new_v4().to_string();

            let payload = serde_json::json!({
                "file_path": chunk.file_path,
                "language": chunk.language,
                "node_type": chunk.node_type,
                "entity_name": chunk.entity_name,
                "line_start": chunk.line_start,
                "line_end": chunk.line_end,
                "code": chunk.code,
                "scope_chain": chunk.scope_chain,
                "content_hash": chunk.content_hash,
            });

            let payload_map = match payload {
                serde_json::Value::Object(m) => m.into_iter().collect(),
                _ => std::collections::HashMap::new(),
            };

            points.push(VectorPoint {
                id: point_id.clone(),
                vector: vector.clone(),
                payload: payload_map,
            });
            point_ids.push(point_id);
        }

        VectorStore::upsert(&self.ops, &self.collection, points).await?;

        let mut tx = self.pool.begin().await?;
        for (idx, (chunk, _)) in chunks.iter().enumerate() {
            let point_id = &point_ids[idx];
            let line_start = i64::try_from(chunk.line_start)?;
            let line_end = i64::try_from(chunk.line_end)?;

            zeph_db::query(
                sql!("INSERT INTO chunk_metadata \
                 (qdrant_id, file_path, content_hash, line_start, line_end, language, node_type, entity_name) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(file_path, content_hash) DO UPDATE SET \
                   qdrant_id = excluded.qdrant_id, \
                   line_start = excluded.line_start, line_end = excluded.line_end, \
                   language = excluded.language, node_type = excluded.node_type, \
                   entity_name = excluded.entity_name"),
            )
            .bind(point_id)
            .bind(chunk.file_path)
            .bind(chunk.content_hash)
            .bind(line_start)
            .bind(line_end)
            .bind(chunk.language)
            .bind(chunk.node_type)
            .bind(chunk.entity_name)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        Ok(point_ids)
    }

    /// Check if a chunk with this content hash already exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` query fails.
    pub async fn chunk_exists(&self, content_hash: &str) -> Result<bool> {
        let row: (i64,) = zeph_db::query_as(sql!(
            "SELECT COUNT(*) FROM chunk_metadata WHERE content_hash = ?"
        ))
        .bind(content_hash)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 > 0)
    }

    /// Return the set of content hashes that already exist in the store.
    ///
    /// Uses `WHERE content_hash IN (...)` with chunks of 900 to stay below
    /// `SQLite`'s default variable limit of 999.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` query fails.
    pub async fn existing_hashes(
        &self,
        hashes: &[&str],
    ) -> Result<std::collections::HashSet<String>> {
        if hashes.is_empty() {
            return Ok(std::collections::HashSet::new());
        }

        let mut result = std::collections::HashSet::new();

        for chunk in hashes.chunks(900) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT content_hash FROM chunk_metadata WHERE content_hash IN ({placeholders})"
            );
            let mut query = zeph_db::query_scalar::<_, String>(&sql);
            for hash in chunk {
                query = query.bind(*hash);
            }
            let rows: Vec<String> = query.fetch_all(&self.pool).await?;
            result.extend(rows);
        }

        Ok(result)
    }

    /// Remove all chunks for a given file path from both stores.
    ///
    /// # Errors
    ///
    /// Returns an error if `Qdrant` or `SQLite` operations fail.
    pub async fn remove_file_chunks(&self, file_path: &str) -> Result<usize> {
        let ids: Vec<(String,)> = zeph_db::query_as(sql!(
            "SELECT qdrant_id FROM chunk_metadata WHERE file_path = ?"
        ))
        .bind(file_path)
        .fetch_all(&self.pool)
        .await?;

        if ids.is_empty() {
            return Ok(0);
        }

        let point_ids: Vec<String> = ids.iter().map(|(id,)| id.clone()).collect();

        VectorStore::delete_by_ids(&self.ops, &self.collection, point_ids).await?;

        let count = ids.len();
        zeph_db::query(sql!("DELETE FROM chunk_metadata WHERE file_path = ?"))
            .bind(file_path)
            .execute(&self.pool)
            .await?;

        Ok(count)
    }

    /// Search for similar code chunks.
    ///
    /// The `query_vector` must be L2-normalized (use
    /// [`EmbeddingVector::<Unnormalized>::normalize`](zeph_common::EmbeddingVector::normalize)
    /// or
    /// [`EmbeddingVector::<Normalized>::new_normalized`](zeph_common::EmbeddingVector::new_normalized)
    /// before calling). Requiring [`Normalized`] at the type level prevents silent
    /// near-zero cosine scores that Qdrant gRPC returns for mismatched or
    /// unnormalized vectors.
    ///
    /// # Errors
    ///
    /// Returns an error if `Qdrant` search fails.
    pub async fn search(
        &self,
        query_vector: EmbeddingVector<Normalized>,
        limit: usize,
        language_filter: Option<String>,
    ) -> Result<Vec<SearchHit>> {
        let limit_u64 = u64::try_from(limit)?;
        let filter = language_filter.map(|lang| VectorFilter {
            must: vec![FieldCondition {
                field: "language".into(),
                value: FieldValue::Text(lang),
            }],
            must_not: vec![],
        });

        let results = VectorStore::search(
            &self.ops,
            &self.collection,
            query_vector.into_inner(),
            limit_u64,
            filter,
        )
        .await?;

        Ok(results
            .into_iter()
            .filter_map(|p| SearchHit::from_payload(&p))
            .collect())
    }

    /// List all indexed file paths.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` query fails.
    pub async fn indexed_files(&self) -> Result<Vec<String>> {
        let rows: Vec<(String,)> =
            zeph_db::query_as(sql!("SELECT DISTINCT file_path FROM chunk_metadata"))
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|(p,)| p).collect())
    }
}

impl SearchHit {
    fn from_payload(point: &zeph_memory::ScoredVectorPoint) -> Option<Self> {
        let get_str = |key: &str| -> Option<String> {
            point
                .payload
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        };
        let get_usize = |key: &str| -> Option<usize> {
            point
                .payload
                .get(key)
                .and_then(serde_json::Value::as_i64)
                .and_then(|v| usize::try_from(v).ok())
        };

        Some(Self {
            code: get_str("code")?,
            file_path: get_str("file_path")?,
            line_range: (get_usize("line_start")?, get_usize("line_end")?),
            score: point.score,
            node_type: get_str("node_type")?,
            entity_name: get_str("entity_name"),
            scope_chain: get_str("scope_chain").unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_memory::ScoredVectorPoint;

    fn make_scored_point(payload: serde_json::Value, score: f32) -> ScoredVectorPoint {
        let map = match payload {
            serde_json::Value::Object(m) => m.into_iter().collect(),
            _ => std::collections::HashMap::new(),
        };
        ScoredVectorPoint {
            id: "test-id".to_string(),
            score,
            payload: map,
        }
    }

    #[test]
    fn search_hit_from_payload_full() {
        let point = make_scored_point(
            serde_json::json!({
                "code": "fn foo() {}",
                "file_path": "src/lib.rs",
                "line_start": 10,
                "line_end": 12,
                "node_type": "function_item",
                "entity_name": "foo",
                "scope_chain": "mod::foo"
            }),
            0.9,
        );
        let hit = SearchHit::from_payload(&point).unwrap();
        assert_eq!(hit.code, "fn foo() {}");
        assert_eq!(hit.file_path, "src/lib.rs");
        assert_eq!(hit.line_range, (10, 12));
        assert!((hit.score - 0.9).abs() < f32::EPSILON);
        assert_eq!(hit.node_type, "function_item");
        assert_eq!(hit.entity_name, Some("foo".to_string()));
        assert_eq!(hit.scope_chain, "mod::foo");
    }

    #[test]
    fn search_hit_from_payload_no_entity_name() {
        let point = make_scored_point(
            serde_json::json!({
                "code": "struct Bar {}",
                "file_path": "src/bar.rs",
                "line_start": 1,
                "line_end": 3,
                "node_type": "struct_item",
                "scope_chain": ""
            }),
            0.7,
        );
        let hit = SearchHit::from_payload(&point).unwrap();
        assert!(hit.entity_name.is_none());
        assert_eq!(hit.node_type, "struct_item");
    }

    #[test]
    fn search_hit_from_payload_missing_required_field_returns_none() {
        // Missing "code" field — should return None
        let point = make_scored_point(
            serde_json::json!({
                "file_path": "src/lib.rs",
                "line_start": 1,
                "line_end": 2,
                "node_type": "function_item"
            }),
            0.5,
        );
        assert!(SearchHit::from_payload(&point).is_none());
    }

    async fn setup_pool() -> zeph_db::DbPool {
        zeph_db::DbConfig {
            url: ":memory:".to_string(),
            ..Default::default()
        }
        .connect()
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn chunk_exists_returns_false_then_true() {
        let pool = setup_pool().await;

        let exists = zeph_db::query_as::<_, (i64,)>(sql!(
            "SELECT COUNT(*) FROM chunk_metadata WHERE content_hash = ?"
        ))
        .bind("abc123")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(exists.0, 0);

        zeph_db::query(sql!(
            "INSERT INTO chunk_metadata \
             (qdrant_id, file_path, content_hash, line_start, line_end, language, node_type) \
             VALUES (?, ?, ?, ?, ?, ?, ?)"
        ))
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

        let exists = zeph_db::query_as::<_, (i64,)>(sql!(
            "SELECT COUNT(*) FROM chunk_metadata WHERE content_hash = ?"
        ))
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
            zeph_db::query(sql!(
                "INSERT INTO chunk_metadata \
                 (qdrant_id, file_path, content_hash, line_start, line_end, language, node_type) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)"
            ))
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

        let ids: Vec<(String,)> = zeph_db::query_as(sql!(
            "SELECT qdrant_id FROM chunk_metadata WHERE file_path = ?"
        ))
        .bind("src/lib.rs")
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(ids.len(), 3);

        zeph_db::query(sql!("DELETE FROM chunk_metadata WHERE file_path = ?"))
            .bind("src/lib.rs")
            .execute(&pool)
            .await
            .unwrap();

        let remaining: (i64,) = zeph_db::query_as(sql!(
            "SELECT COUNT(*) FROM chunk_metadata WHERE file_path = ?"
        ))
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
            zeph_db::query(sql!(
                "INSERT INTO chunk_metadata \
                 (qdrant_id, file_path, content_hash, line_start, line_end, language, node_type) \
                 VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(qdrant_id) DO UPDATE SET \
                   file_path = excluded.file_path, content_hash = excluded.content_hash, \
                   line_start = excluded.line_start, line_end = excluded.line_end, \
                   language = excluded.language, node_type = excluded.node_type"
            ))
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

        let rows: Vec<(String,)> =
            zeph_db::query_as(sql!("SELECT DISTINCT file_path FROM chunk_metadata"))
                .fetch_all(&pool)
                .await
                .unwrap();
        let files: Vec<String> = rows.into_iter().map(|(p,)| p).collect();
        assert_eq!(files.len(), 2);
        assert!(files.contains(&"src/a.rs".to_string()));
        assert!(files.contains(&"src/b.rs".to_string()));
    }

    /// Verifies that inserting the same (`file_path`, `content_hash`) twice does not
    /// produce a duplicate row — the `ON CONFLICT(file_path, content_hash)` clause
    /// must perform an UPDATE, not a second INSERT.
    #[tokio::test]
    async fn upsert_same_file_path_and_hash_is_idempotent() {
        let pool = setup_pool().await;

        for i in 0..2_u32 {
            zeph_db::query(sql!(
                "INSERT INTO chunk_metadata \
                 (qdrant_id, file_path, content_hash, line_start, line_end, language, node_type) \
                 VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(file_path, content_hash) DO UPDATE SET \
                   qdrant_id = excluded.qdrant_id, \
                   line_start = excluded.line_start, line_end = excluded.line_end, \
                   language = excluded.language, node_type = excluded.node_type, \
                   entity_name = excluded.entity_name"
            ))
            .bind(format!("q{i}"))
            .bind("src/lib.rs")
            .bind("dedup_hash")
            .bind(1_i64)
            .bind(5_i64)
            .bind("rust")
            .bind("function_item")
            .execute(&pool)
            .await
            .unwrap();
        }

        let count: (i64,) = zeph_db::query_as(sql!(
            "SELECT COUNT(*) FROM chunk_metadata \
             WHERE file_path = 'src/lib.rs' AND content_hash = 'dedup_hash'"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(count.0, 1, "duplicate upsert must not produce a second row");

        // The second upsert must have updated qdrant_id to the latest value.
        let qdrant_id: (String,) = zeph_db::query_as(sql!(
            "SELECT qdrant_id FROM chunk_metadata \
             WHERE file_path = 'src/lib.rs' AND content_hash = 'dedup_hash'"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            qdrant_id.0, "q1",
            "qdrant_id must reflect the latest upsert"
        );
    }

    #[tokio::test]
    async fn existing_hashes_empty_input_returns_empty_set() {
        let pool = setup_pool().await;
        let ops = zeph_memory::QdrantOps::new("http://127.0.0.1:1", None).unwrap();
        let store = CodeStore::with_ops(ops, pool);
        let result = store.existing_hashes(&[]).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn existing_hashes_chunking_above_900() {
        let pool = setup_pool().await;

        // Insert 901 rows.
        for i in 0..901_usize {
            zeph_db::query(sql!(
                "INSERT INTO chunk_metadata \
                 (qdrant_id, file_path, content_hash, line_start, line_end, language, node_type) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)"
            ))
            .bind(format!("q{i}"))
            .bind("src/lib.rs")
            .bind(format!("hash{i:04}"))
            .bind(1_i64)
            .bind(2_i64)
            .bind("rust")
            .bind("function_item")
            .execute(&pool)
            .await
            .unwrap();
        }

        let all_hashes: Vec<String> = (0..901).map(|i| format!("hash{i:04}")).collect();
        let refs: Vec<&str> = all_hashes.iter().map(String::as_str).collect();

        let ops = zeph_memory::QdrantOps::new("http://127.0.0.1:1", None).unwrap();
        let store = CodeStore::with_ops(ops, pool);
        let result = store.existing_hashes(&refs).await.unwrap();

        assert_eq!(result.len(), 901);
        // Spot-check a few entries.
        assert!(result.contains("hash0000"));
        assert!(result.contains("hash0900"));
    }
}
