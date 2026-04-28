// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `SQLite` BLOB vector store — offline fallback implementation.
//!
//! Stores dense vectors as raw `f32` BLOBs in a `SQLite` table and performs cosine
//! similarity in memory.  Suitable for offline use and CI environments without a
//! running Qdrant instance.  Not optimised for large collections.

use std::collections::HashMap;
#[allow(unused_imports)]
use zeph_db::sql;

use zeph_db::{ActiveDialect, DbPool};

use crate::vector_store::{
    BoxFuture, FieldValue, ScoredVectorPoint, ScrollResult, ScrollWithIdsResult, VectorFilter,
    VectorPoint, VectorStore, VectorStoreError,
};

/// Database-backed in-process vector store.
///
/// Stores vectors as BLOBs in `SQLite` and performs cosine similarity in memory.
/// For production-scale workloads, prefer the Qdrant-backed store.
pub struct DbVectorStore {
    pool: DbPool,
}

/// Backward-compatible alias.
pub type SqliteVectorStore = DbVectorStore;

impl DbVectorStore {
    /// Create a new `DbVectorStore` from an existing connection pool.
    ///
    /// The pool must come from a database that has run the `zeph-db` migrations
    /// (which create the `vector_store` table).
    #[must_use]
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

use zeph_common::math::cosine_similarity;

fn matches_filter(payload: &HashMap<String, serde_json::Value>, filter: &VectorFilter) -> bool {
    for cond in &filter.must {
        let Some(val) = payload.get(&cond.field) else {
            return false;
        };
        let matches = match &cond.value {
            FieldValue::Integer(i) => val.as_i64().is_some_and(|v| v == *i),
            FieldValue::Text(t) => val.as_str().is_some_and(|v| v == t.as_str()),
        };
        if !matches {
            return false;
        }
    }
    for cond in &filter.must_not {
        let Some(val) = payload.get(&cond.field) else {
            continue;
        };
        let matches = match &cond.value {
            FieldValue::Integer(i) => val.as_i64().is_some_and(|v| v == *i),
            FieldValue::Text(t) => val.as_str().is_some_and(|v| v == t.as_str()),
        };
        if matches {
            return false;
        }
    }
    true
}

impl VectorStore for DbVectorStore {
    fn ensure_collection(
        &self,
        collection: &str,
        _vector_size: u64,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            let sql = format!(
                "{} INTO vector_collections (name) VALUES (?){}",
                <ActiveDialect as zeph_db::dialect::Dialect>::INSERT_IGNORE,
                <ActiveDialect as zeph_db::dialect::Dialect>::CONFLICT_NOTHING,
            );
            zeph_db::query(&sql)
                .bind(&collection)
                .execute(&self.pool)
                .await
                .map_err(|e| VectorStoreError::Collection(e.to_string()))?;
            Ok(())
        })
    }

    fn collection_exists(&self, collection: &str) -> BoxFuture<'_, Result<bool, VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            let row: (i64,) = zeph_db::query_as(sql!(
                "SELECT COUNT(*) FROM vector_collections WHERE name = ?"
            ))
            .bind(&collection)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| VectorStoreError::Connection(e.to_string()))?;
            Ok(row.0 > 0)
        })
    }

    fn delete_collection(&self, collection: &str) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            zeph_db::query(sql!("DELETE FROM vector_points WHERE collection = ?"))
                .bind(&collection)
                .execute(&self.pool)
                .await
                .map_err(|e| VectorStoreError::Delete(e.to_string()))?;
            zeph_db::query(sql!("DELETE FROM vector_collections WHERE name = ?"))
                .bind(&collection)
                .execute(&self.pool)
                .await
                .map_err(|e| VectorStoreError::Delete(e.to_string()))?;
            Ok(())
        })
    }

    fn upsert(
        &self,
        collection: &str,
        points: Vec<VectorPoint>,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        let collection = collection.to_owned();
        #[cfg(feature = "profiling")]
        let span = tracing::info_span!(
            "memory.vector_store",
            operation = "upsert",
            collection = %collection
        );
        let fut = Box::pin(async move {
            for point in points {
                let vector_bytes: Vec<u8> =
                    point.vector.iter().flat_map(|f| f.to_le_bytes()).collect();
                let payload_json = serde_json::to_string(&point.payload)
                    .map_err(|e| VectorStoreError::Serialization(e.to_string()))?;
                zeph_db::query(
                    sql!("INSERT INTO vector_points (id, collection, vector, payload) VALUES (?, ?, ?, ?) \
                     ON CONFLICT(collection, id) DO UPDATE SET vector = excluded.vector, payload = excluded.payload"),
                )
                .bind(&point.id)
                .bind(&collection)
                .bind(&vector_bytes)
                .bind(&payload_json)
                .execute(&self.pool)
                .await
                .map_err(|e| VectorStoreError::Upsert(e.to_string()))?;
            }
            Ok(())
        });
        #[cfg(feature = "profiling")]
        return Box::pin(tracing::Instrument::instrument(fut, span));
        #[cfg(not(feature = "profiling"))]
        fut
    }

    fn search(
        &self,
        collection: &str,
        vector: Vec<f32>,
        limit: u64,
        filter: Option<VectorFilter>,
    ) -> BoxFuture<'_, Result<Vec<ScoredVectorPoint>, VectorStoreError>> {
        let collection = collection.to_owned();
        #[cfg(feature = "profiling")]
        let span = tracing::info_span!(
            "memory.vector_store",
            operation = "search",
            collection = %collection
        );
        let fut = Box::pin(async move {
            let rows: Vec<(String, Vec<u8>, String)> = zeph_db::query_as(sql!(
                "SELECT id, vector, payload FROM vector_points WHERE collection = ?"
            ))
            .bind(&collection)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| VectorStoreError::Search(e.to_string()))?;

            let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);
            let mut scored: Vec<ScoredVectorPoint> = rows
                .into_iter()
                .filter_map(|(id, blob, payload_str)| {
                    if blob.len() % 4 != 0 {
                        return None;
                    }
                    let stored: Vec<f32> = blob
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                        .collect();
                    let payload: HashMap<String, serde_json::Value> =
                        serde_json::from_str(&payload_str).unwrap_or_default();

                    if filter
                        .as_ref()
                        .is_some_and(|f| !matches_filter(&payload, f))
                    {
                        return None;
                    }

                    let score = cosine_similarity(&vector, &stored);
                    Some(ScoredVectorPoint { id, score, payload })
                })
                .collect();

            scored.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            scored.truncate(limit_usize);
            Ok(scored)
        });
        #[cfg(feature = "profiling")]
        return Box::pin(tracing::Instrument::instrument(fut, span));
        #[cfg(not(feature = "profiling"))]
        fut
    }

    fn delete_by_ids(
        &self,
        collection: &str,
        ids: Vec<String>,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        let collection = collection.to_owned();
        #[cfg(feature = "profiling")]
        let span = tracing::info_span!(
            "memory.vector_store",
            operation = "delete",
            collection = %collection
        );
        let fut = Box::pin(async move {
            for id in ids {
                zeph_db::query(sql!(
                    "DELETE FROM vector_points WHERE collection = ? AND id = ?"
                ))
                .bind(&collection)
                .bind(&id)
                .execute(&self.pool)
                .await
                .map_err(|e| VectorStoreError::Delete(e.to_string()))?;
            }
            Ok(())
        });
        #[cfg(feature = "profiling")]
        return Box::pin(tracing::Instrument::instrument(fut, span));
        #[cfg(not(feature = "profiling"))]
        fut
    }

    fn scroll_all(
        &self,
        collection: &str,
        key_field: &str,
    ) -> BoxFuture<'_, Result<ScrollResult, VectorStoreError>> {
        let collection = collection.to_owned();
        let key_field = key_field.to_owned();
        Box::pin(async move {
            let rows: Vec<(String, String)> = zeph_db::query_as(sql!(
                "SELECT id, payload FROM vector_points WHERE collection = ?"
            ))
            .bind(&collection)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| VectorStoreError::Scroll(e.to_string()))?;

            let mut result = ScrollResult::new();
            for (id, payload_str) in rows {
                let payload: HashMap<String, serde_json::Value> =
                    serde_json::from_str(&payload_str).unwrap_or_default();
                if let Some(val) = payload.get(&key_field) {
                    let mut map = HashMap::new();
                    map.insert(
                        key_field.clone(),
                        val.as_str().unwrap_or_default().to_owned(),
                    );
                    result.insert(id, map);
                }
            }
            Ok(result)
        })
    }

    fn scroll_all_with_point_ids(
        &self,
        collection: &str,
        key_field: &str,
    ) -> BoxFuture<'_, Result<ScrollWithIdsResult, VectorStoreError>> {
        let collection = collection.to_owned();
        let key_field = key_field.to_owned();
        Box::pin(async move {
            let rows: Vec<(String, String)> = zeph_db::query_as(sql!(
                "SELECT id, payload FROM vector_points WHERE collection = ?"
            ))
            .bind(&collection)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| VectorStoreError::Scroll(e.to_string()))?;

            let mut result = Vec::new();
            for (point_id, payload_str) in rows {
                let payload: HashMap<String, serde_json::Value> =
                    serde_json::from_str(&payload_str).unwrap_or_default();
                let Some(key_val) = payload.get(&key_field).and_then(|v| v.as_str()) else {
                    continue;
                };
                let mut fields = HashMap::new();
                for (k, v) in &payload {
                    if let Some(s) = v.as_str() {
                        fields.insert(k.clone(), s.to_owned());
                    }
                }
                // Ensure the key_field value is always present in the fields map.
                fields.insert(key_field.clone(), key_val.to_owned());
                result.push((point_id, fields));
            }
            Ok(result)
        })
    }

    fn health_check(&self) -> BoxFuture<'_, Result<bool, VectorStoreError>> {
        Box::pin(async move {
            zeph_db::query_scalar::<_, i32>(sql!("SELECT 1"))
                .fetch_one(&self.pool)
                .await
                .map(|_| true)
                .map_err(|e| VectorStoreError::Collection(e.to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::DbStore;
    use crate::vector_store::FieldCondition;

    async fn setup() -> (DbVectorStore, DbStore) {
        let store = DbStore::new(":memory:").await.unwrap();
        let pool = store.pool().clone();
        let vs = DbVectorStore::new(pool);
        (vs, store)
    }

    #[tokio::test]
    async fn ensure_and_exists() {
        let (vs, _) = setup().await;
        assert!(!vs.collection_exists("col1").await.unwrap());
        vs.ensure_collection("col1", 4).await.unwrap();
        assert!(vs.collection_exists("col1").await.unwrap());
        // idempotent
        vs.ensure_collection("col1", 4).await.unwrap();
        assert!(vs.collection_exists("col1").await.unwrap());
    }

    #[tokio::test]
    async fn delete_collection() {
        let (vs, _) = setup().await;
        vs.ensure_collection("col1", 4).await.unwrap();
        vs.upsert(
            "col1",
            vec![VectorPoint {
                id: "p1".into(),
                vector: vec![1.0, 0.0, 0.0, 0.0],
                payload: HashMap::new(),
            }],
        )
        .await
        .unwrap();
        vs.delete_collection("col1").await.unwrap();
        assert!(!vs.collection_exists("col1").await.unwrap());
    }

    #[tokio::test]
    async fn upsert_and_search() {
        let (vs, _) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();
        vs.upsert(
            "c",
            vec![
                VectorPoint {
                    id: "a".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::from([("role".into(), serde_json::json!("user"))]),
                },
                VectorPoint {
                    id: "b".into(),
                    vector: vec![0.0, 1.0, 0.0, 0.0],
                    payload: HashMap::from([("role".into(), serde_json::json!("assistant"))]),
                },
            ],
        )
        .await
        .unwrap();

        let results = vs
            .search("c", vec![1.0, 0.0, 0.0, 0.0], 2, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "a");
        assert!((results[0].score - 1.0).abs() < 1e-5);
    }

    #[tokio::test]
    async fn search_with_filter() {
        let (vs, _) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();
        vs.upsert(
            "c",
            vec![
                VectorPoint {
                    id: "a".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::from([("role".into(), serde_json::json!("user"))]),
                },
                VectorPoint {
                    id: "b".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::from([("role".into(), serde_json::json!("assistant"))]),
                },
            ],
        )
        .await
        .unwrap();

        let filter = VectorFilter {
            must: vec![FieldCondition {
                field: "role".into(),
                value: FieldValue::Text("user".into()),
            }],
            must_not: vec![],
        };
        let results = vs
            .search("c", vec![1.0, 0.0, 0.0, 0.0], 10, Some(filter))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a");
    }

    #[tokio::test]
    async fn delete_by_ids() {
        let (vs, _) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();
        vs.upsert(
            "c",
            vec![
                VectorPoint {
                    id: "a".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::new(),
                },
                VectorPoint {
                    id: "b".into(),
                    vector: vec![0.0, 1.0, 0.0, 0.0],
                    payload: HashMap::new(),
                },
            ],
        )
        .await
        .unwrap();
        vs.delete_by_ids("c", vec!["a".into()]).await.unwrap();
        let results = vs
            .search("c", vec![1.0, 0.0, 0.0, 0.0], 10, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "b");
    }

    #[tokio::test]
    async fn scroll_all() {
        let (vs, _) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();
        vs.upsert(
            "c",
            vec![VectorPoint {
                id: "p1".into(),
                vector: vec![1.0, 0.0, 0.0, 0.0],
                payload: HashMap::from([("text".into(), serde_json::json!("hello"))]),
            }],
        )
        .await
        .unwrap();
        let result = vs.scroll_all("c", "text").await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result["p1"]["text"], "hello");
    }

    #[tokio::test]
    async fn upsert_updates_existing() {
        let (vs, _) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();
        vs.upsert(
            "c",
            vec![VectorPoint {
                id: "p1".into(),
                vector: vec![1.0, 0.0, 0.0, 0.0],
                payload: HashMap::from([("v".into(), serde_json::json!(1))]),
            }],
        )
        .await
        .unwrap();
        vs.upsert(
            "c",
            vec![VectorPoint {
                id: "p1".into(),
                vector: vec![0.0, 1.0, 0.0, 0.0],
                payload: HashMap::from([("v".into(), serde_json::json!(2))]),
            }],
        )
        .await
        .unwrap();
        let results = vs
            .search("c", vec![0.0, 1.0, 0.0, 0.0], 1, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!((results[0].score - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_similarity_import_wired() {
        // Smoke test: verifies the re-export binding is intact. Edge-case coverage is in math.rs.
        assert!(!cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).is_nan());
    }

    #[tokio::test]
    async fn search_with_must_not_filter() {
        let (vs, _) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();
        vs.upsert(
            "c",
            vec![
                VectorPoint {
                    id: "a".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::from([("role".into(), serde_json::json!("user"))]),
                },
                VectorPoint {
                    id: "b".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::from([("role".into(), serde_json::json!("system"))]),
                },
            ],
        )
        .await
        .unwrap();

        let filter = VectorFilter {
            must: vec![],
            must_not: vec![FieldCondition {
                field: "role".into(),
                value: FieldValue::Text("system".into()),
            }],
        };
        let results = vs
            .search("c", vec![1.0, 0.0, 0.0, 0.0], 10, Some(filter))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a");
    }

    #[tokio::test]
    async fn search_with_integer_filter() {
        let (vs, _) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();
        vs.upsert(
            "c",
            vec![
                VectorPoint {
                    id: "a".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::from([("conv_id".into(), serde_json::json!(1))]),
                },
                VectorPoint {
                    id: "b".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::from([("conv_id".into(), serde_json::json!(2))]),
                },
            ],
        )
        .await
        .unwrap();

        let filter = VectorFilter {
            must: vec![FieldCondition {
                field: "conv_id".into(),
                value: FieldValue::Integer(1),
            }],
            must_not: vec![],
        };
        let results = vs
            .search("c", vec![1.0, 0.0, 0.0, 0.0], 10, Some(filter))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a");
    }

    #[tokio::test]
    async fn search_empty_collection() {
        let (vs, _) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();
        let results = vs
            .search("c", vec![1.0, 0.0, 0.0, 0.0], 10, None)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_with_must_not_integer_filter() {
        let (vs, _) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();
        vs.upsert(
            "c",
            vec![
                VectorPoint {
                    id: "a".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::from([("conv_id".into(), serde_json::json!(1))]),
                },
                VectorPoint {
                    id: "b".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::from([("conv_id".into(), serde_json::json!(2))]),
                },
            ],
        )
        .await
        .unwrap();

        let filter = VectorFilter {
            must: vec![],
            must_not: vec![FieldCondition {
                field: "conv_id".into(),
                value: FieldValue::Integer(1),
            }],
        };
        let results = vs
            .search("c", vec![1.0, 0.0, 0.0, 0.0], 10, Some(filter))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "b");
    }

    #[tokio::test]
    async fn search_with_combined_must_and_must_not() {
        let (vs, _) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();
        vs.upsert(
            "c",
            vec![
                VectorPoint {
                    id: "a".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::from([
                        ("role".into(), serde_json::json!("user")),
                        ("conv_id".into(), serde_json::json!(1)),
                    ]),
                },
                VectorPoint {
                    id: "b".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::from([
                        ("role".into(), serde_json::json!("user")),
                        ("conv_id".into(), serde_json::json!(2)),
                    ]),
                },
                VectorPoint {
                    id: "c".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::from([
                        ("role".into(), serde_json::json!("assistant")),
                        ("conv_id".into(), serde_json::json!(1)),
                    ]),
                },
            ],
        )
        .await
        .unwrap();

        let filter = VectorFilter {
            must: vec![FieldCondition {
                field: "role".into(),
                value: FieldValue::Text("user".into()),
            }],
            must_not: vec![FieldCondition {
                field: "conv_id".into(),
                value: FieldValue::Integer(2),
            }],
        };
        let results = vs
            .search("c", vec![1.0, 0.0, 0.0, 0.0], 10, Some(filter))
            .await
            .unwrap();
        // Only "a": role=user AND conv_id != 2
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a");
    }

    #[tokio::test]
    async fn scroll_all_missing_key_field() {
        let (vs, _) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();
        vs.upsert(
            "c",
            vec![VectorPoint {
                id: "p1".into(),
                vector: vec![1.0, 0.0, 0.0, 0.0],
                payload: HashMap::from([("other".into(), serde_json::json!("value"))]),
            }],
        )
        .await
        .unwrap();
        // key_field "text" doesn't exist in payload → point excluded from result
        let result = vs.scroll_all("c", "text").await.unwrap();
        assert!(
            result.is_empty(),
            "points without the key field must not appear in scroll result"
        );
    }

    #[tokio::test]
    async fn delete_by_ids_empty_and_nonexistent() {
        let (vs, _) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();
        vs.upsert(
            "c",
            vec![VectorPoint {
                id: "a".into(),
                vector: vec![1.0, 0.0, 0.0, 0.0],
                payload: HashMap::new(),
            }],
        )
        .await
        .unwrap();

        // Empty list: no-op, must succeed
        vs.delete_by_ids("c", vec![]).await.unwrap();

        // Non-existent id: must succeed (idempotent)
        vs.delete_by_ids("c", vec!["nonexistent".into()])
            .await
            .unwrap();

        // Original point still present
        let results = vs
            .search("c", vec![1.0, 0.0, 0.0, 0.0], 10, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a");
    }

    #[tokio::test]
    async fn search_corrupt_blob_skipped() {
        let (vs, store) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();

        // Insert a valid point first
        vs.upsert(
            "c",
            vec![VectorPoint {
                id: "valid".into(),
                vector: vec![1.0, 0.0, 0.0, 0.0],
                payload: HashMap::new(),
            }],
        )
        .await
        .unwrap();

        // Insert raw invalid bytes directly into vector_points table
        // 3 bytes cannot be cast to f32 (needs multiples of 4)
        let corrupt_blob: Vec<u8> = vec![0xFF, 0xFE, 0xFD];
        let payload_json = r"{}";
        zeph_db::query(sql!(
            "INSERT INTO vector_points (id, collection, vector, payload) VALUES (?, ?, ?, ?)"
        ))
        .bind("corrupt")
        .bind("c")
        .bind(&corrupt_blob)
        .bind(payload_json)
        .execute(store.pool())
        .await
        .unwrap();

        // Search must not panic and must skip the corrupt point
        let results = vs
            .search("c", vec![1.0, 0.0, 0.0, 0.0], 10, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "valid");
    }

    #[tokio::test]
    async fn scroll_all_with_point_ids_basic() {
        let (vs, _) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();
        vs.upsert(
            "c",
            vec![
                VectorPoint {
                    id: "p1".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::from([
                        ("entity_id_str".into(), serde_json::json!("42")),
                        ("name".into(), serde_json::json!("alice")),
                    ]),
                },
                VectorPoint {
                    id: "p2".into(),
                    vector: vec![0.0, 1.0, 0.0, 0.0],
                    payload: HashMap::from([
                        ("entity_id_str".into(), serde_json::json!("99")),
                        ("name".into(), serde_json::json!("bob")),
                    ]),
                },
            ],
        )
        .await
        .unwrap();

        let result = vs
            .scroll_all_with_point_ids("c", "entity_id_str")
            .await
            .unwrap();
        assert_eq!(result.len(), 2);

        // Collect into a sorted map for deterministic assertion
        let mut by_id: std::collections::BTreeMap<
            String,
            std::collections::HashMap<String, String>,
        > = result.into_iter().collect();
        let p1 = by_id.remove("p1").expect("p1 missing");
        let p2 = by_id.remove("p2").expect("p2 missing");
        assert_eq!(p1.get("entity_id_str").map(String::as_str), Some("42"));
        assert_eq!(p1.get("name").map(String::as_str), Some("alice"));
        assert_eq!(p2.get("entity_id_str").map(String::as_str), Some("99"));
        assert_eq!(p2.get("name").map(String::as_str), Some("bob"));
    }

    #[tokio::test]
    async fn scroll_all_with_point_ids_skips_missing_key_field() {
        let (vs, _) = setup().await;
        vs.ensure_collection("c", 4).await.unwrap();
        vs.upsert(
            "c",
            vec![
                VectorPoint {
                    id: "has-key".into(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    payload: HashMap::from([("entity_id_str".into(), serde_json::json!("7"))]),
                },
                VectorPoint {
                    id: "no-key".into(),
                    vector: vec![0.0, 1.0, 0.0, 0.0],
                    payload: HashMap::from([("other".into(), serde_json::json!("value"))]),
                },
            ],
        )
        .await
        .unwrap();

        let result = vs
            .scroll_all_with_point_ids("c", "entity_id_str")
            .await
            .unwrap();
        // Only the point that has the key field must be returned
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "has-key");
        assert_eq!(
            result[0].1.get("entity_id_str").map(String::as_str),
            Some("7")
        );
    }
}
