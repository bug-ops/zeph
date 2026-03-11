// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use sqlx::SqlitePool;

use crate::vector_store::{
    FieldValue, ScoredVectorPoint, ScrollResult, VectorFilter, VectorPoint, VectorStore,
    VectorStoreError,
};

pub struct SqliteVectorStore {
    pool: SqlitePool,
}

impl SqliteVectorStore {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

use crate::math::cosine_similarity;

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

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl VectorStore for SqliteVectorStore {
    fn ensure_collection(
        &self,
        collection: &str,
        _vector_size: u64,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            sqlx::query("INSERT OR IGNORE INTO vector_collections (name) VALUES (?)")
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
            let row: (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM vector_collections WHERE name = ?")
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
            sqlx::query("DELETE FROM vector_points WHERE collection = ?")
                .bind(&collection)
                .execute(&self.pool)
                .await
                .map_err(|e| VectorStoreError::Delete(e.to_string()))?;
            sqlx::query("DELETE FROM vector_collections WHERE name = ?")
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
        Box::pin(async move {
            for point in points {
                let vector_bytes: Vec<u8> = bytemuck::cast_slice(&point.vector).to_vec();
                let payload_json = serde_json::to_string(&point.payload)
                    .map_err(|e| VectorStoreError::Serialization(e.to_string()))?;
                sqlx::query(
                    "INSERT INTO vector_points (id, collection, vector, payload) VALUES (?, ?, ?, ?) \
                     ON CONFLICT(collection, id) DO UPDATE SET vector = excluded.vector, payload = excluded.payload",
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
        })
    }

    fn search(
        &self,
        collection: &str,
        vector: Vec<f32>,
        limit: u64,
        filter: Option<VectorFilter>,
    ) -> BoxFuture<'_, Result<Vec<ScoredVectorPoint>, VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            let rows: Vec<(String, Vec<u8>, String)> = sqlx::query_as(
                "SELECT id, vector, payload FROM vector_points WHERE collection = ?",
            )
            .bind(&collection)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| VectorStoreError::Search(e.to_string()))?;

            let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);
            let mut scored: Vec<ScoredVectorPoint> = rows
                .into_iter()
                .filter_map(|(id, blob, payload_str)| {
                    let Ok(stored) = bytemuck::try_cast_slice::<u8, f32>(&blob) else {
                        return None;
                    };
                    let payload: HashMap<String, serde_json::Value> =
                        serde_json::from_str(&payload_str).unwrap_or_default();

                    if filter
                        .as_ref()
                        .is_some_and(|f| !matches_filter(&payload, f))
                    {
                        return None;
                    }

                    let score = cosine_similarity(&vector, stored);
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
        })
    }

    fn delete_by_ids(
        &self,
        collection: &str,
        ids: Vec<String>,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            for id in ids {
                sqlx::query("DELETE FROM vector_points WHERE collection = ? AND id = ?")
                    .bind(&collection)
                    .bind(&id)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| VectorStoreError::Delete(e.to_string()))?;
            }
            Ok(())
        })
    }

    fn scroll_all(
        &self,
        collection: &str,
        key_field: &str,
    ) -> BoxFuture<'_, Result<ScrollResult, VectorStoreError>> {
        let collection = collection.to_owned();
        let key_field = key_field.to_owned();
        Box::pin(async move {
            let rows: Vec<(String, String)> =
                sqlx::query_as("SELECT id, payload FROM vector_points WHERE collection = ?")
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

    fn health_check(&self) -> BoxFuture<'_, Result<bool, VectorStoreError>> {
        Box::pin(async move {
            sqlx::query_scalar::<_, i32>("SELECT 1")
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
    use crate::sqlite::SqliteStore;
    use crate::vector_store::FieldCondition;

    async fn setup() -> (SqliteVectorStore, SqliteStore) {
        let store = SqliteStore::new(":memory:").await.unwrap();
        let pool = store.pool().clone();
        let vs = SqliteVectorStore::new(pool);
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
        sqlx::query(
            "INSERT INTO vector_points (id, collection, vector, payload) VALUES (?, ?, ?, ?)",
        )
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
}
