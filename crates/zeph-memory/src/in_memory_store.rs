use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::RwLock;

use crate::vector_store::{
    FieldValue, ScoredVectorPoint, VectorFilter, VectorPoint, VectorStore, VectorStoreError,
};

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

struct StoredPoint {
    vector: Vec<f32>,
    payload: HashMap<String, serde_json::Value>,
}

struct InMemoryCollection {
    points: HashMap<String, StoredPoint>,
}

pub struct InMemoryVectorStore {
    collections: RwLock<HashMap<String, InMemoryCollection>>,
}

impl InMemoryVectorStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            collections: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryVectorStore {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for InMemoryVectorStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryVectorStore")
            .finish_non_exhaustive()
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

fn matches_filter(payload: &HashMap<String, serde_json::Value>, filter: &VectorFilter) -> bool {
    for cond in &filter.must {
        let Some(val) = payload.get(&cond.field) else {
            return false;
        };
        if !field_matches(val, &cond.value) {
            return false;
        }
    }
    for cond in &filter.must_not {
        if let Some(val) = payload.get(&cond.field)
            && field_matches(val, &cond.value)
        {
            return false;
        }
    }
    true
}

fn field_matches(val: &serde_json::Value, expected: &FieldValue) -> bool {
    match expected {
        FieldValue::Integer(i) => val.as_i64() == Some(*i),
        FieldValue::Text(s) => val.as_str() == Some(s.as_str()),
    }
}

impl VectorStore for InMemoryVectorStore {
    fn ensure_collection(
        &self,
        collection: &str,
        _vector_size: u64,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            let mut cols = self
                .collections
                .write()
                .map_err(|e| VectorStoreError::Collection(e.to_string()))?;
            cols.entry(collection)
                .or_insert_with(|| InMemoryCollection {
                    points: HashMap::new(),
                });
            Ok(())
        })
    }

    fn collection_exists(&self, collection: &str) -> BoxFuture<'_, Result<bool, VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            let cols = self
                .collections
                .read()
                .map_err(|e| VectorStoreError::Collection(e.to_string()))?;
            Ok(cols.contains_key(&collection))
        })
    }

    fn delete_collection(&self, collection: &str) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        let collection = collection.to_owned();
        Box::pin(async move {
            let mut cols = self
                .collections
                .write()
                .map_err(|e| VectorStoreError::Collection(e.to_string()))?;
            cols.remove(&collection);
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
            let mut cols = self
                .collections
                .write()
                .map_err(|e| VectorStoreError::Upsert(e.to_string()))?;
            let col = cols.get_mut(&collection).ok_or_else(|| {
                VectorStoreError::Upsert(format!("collection {collection} not found"))
            })?;
            for p in points {
                col.points.insert(
                    p.id,
                    StoredPoint {
                        vector: p.vector,
                        payload: p.payload,
                    },
                );
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
            let cols = self
                .collections
                .read()
                .map_err(|e| VectorStoreError::Search(e.to_string()))?;
            let col = cols.get(&collection).ok_or_else(|| {
                VectorStoreError::Search(format!("collection {collection} not found"))
            })?;

            let empty_filter = VectorFilter::default();
            let f = filter.as_ref().unwrap_or(&empty_filter);

            let mut scored: Vec<ScoredVectorPoint> = col
                .points
                .iter()
                .filter(|(_, sp)| matches_filter(&sp.payload, f))
                .map(|(id, sp)| ScoredVectorPoint {
                    id: id.clone(),
                    score: cosine_similarity(&vector, &sp.vector),
                    payload: sp.payload.clone(),
                })
                .collect();

            scored.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            #[expect(clippy::cast_possible_truncation)]
            scored.truncate(limit as usize);
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
            if ids.is_empty() {
                return Ok(());
            }
            let mut cols = self
                .collections
                .write()
                .map_err(|e| VectorStoreError::Delete(e.to_string()))?;
            let col = cols.get_mut(&collection).ok_or_else(|| {
                VectorStoreError::Delete(format!("collection {collection} not found"))
            })?;
            for id in &ids {
                col.points.remove(id);
            }
            Ok(())
        })
    }

    fn scroll_all(
        &self,
        collection: &str,
        key_field: &str,
    ) -> BoxFuture<'_, Result<HashMap<String, HashMap<String, String>>, VectorStoreError>> {
        let collection = collection.to_owned();
        let key_field = key_field.to_owned();
        Box::pin(async move {
            let cols = self
                .collections
                .read()
                .map_err(|e| VectorStoreError::Scroll(e.to_string()))?;
            let col = cols.get(&collection).ok_or_else(|| {
                VectorStoreError::Scroll(format!("collection {collection} not found"))
            })?;

            let mut result = HashMap::new();
            for sp in col.points.values() {
                let Some(key_val) = sp.payload.get(&key_field).and_then(|v| v.as_str()) else {
                    continue;
                };
                let mut fields = HashMap::new();
                for (k, v) in &sp.payload {
                    if let Some(s) = v.as_str() {
                        fields.insert(k.clone(), s.to_owned());
                    }
                }
                result.insert(key_val.to_owned(), fields);
            }
            Ok(result)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ensure_collection_and_exists() {
        let store = InMemoryVectorStore::new();
        assert!(!store.collection_exists("test").await.unwrap());
        store.ensure_collection("test", 3).await.unwrap();
        assert!(store.collection_exists("test").await.unwrap());
    }

    #[tokio::test]
    async fn ensure_collection_idempotent() {
        let store = InMemoryVectorStore::new();
        store.ensure_collection("test", 3).await.unwrap();
        store.ensure_collection("test", 3).await.unwrap();
        assert!(store.collection_exists("test").await.unwrap());
    }

    #[tokio::test]
    async fn delete_collection_removes() {
        let store = InMemoryVectorStore::new();
        store.ensure_collection("test", 3).await.unwrap();
        store.delete_collection("test").await.unwrap();
        assert!(!store.collection_exists("test").await.unwrap());
    }

    #[tokio::test]
    async fn upsert_and_search() {
        let store = InMemoryVectorStore::new();
        store.ensure_collection("test", 3).await.unwrap();

        let points = vec![
            VectorPoint {
                id: "a".into(),
                vector: vec![1.0, 0.0, 0.0],
                payload: HashMap::from([("name".into(), serde_json::json!("alpha"))]),
            },
            VectorPoint {
                id: "b".into(),
                vector: vec![0.0, 1.0, 0.0],
                payload: HashMap::from([("name".into(), serde_json::json!("beta"))]),
            },
        ];
        store.upsert("test", points).await.unwrap();

        let results = store
            .search("test", vec![1.0, 0.0, 0.0], 2, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "a");
        assert!((results[0].score - 1.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn search_with_filter() {
        let store = InMemoryVectorStore::new();
        store.ensure_collection("test", 3).await.unwrap();

        let points = vec![
            VectorPoint {
                id: "a".into(),
                vector: vec![1.0, 0.0, 0.0],
                payload: HashMap::from([("role".into(), serde_json::json!("user"))]),
            },
            VectorPoint {
                id: "b".into(),
                vector: vec![0.9, 0.1, 0.0],
                payload: HashMap::from([("role".into(), serde_json::json!("assistant"))]),
            },
        ];
        store.upsert("test", points).await.unwrap();

        let filter = VectorFilter {
            must: vec![crate::vector_store::FieldCondition {
                field: "role".into(),
                value: FieldValue::Text("user".into()),
            }],
            must_not: vec![],
        };
        let results = store
            .search("test", vec![1.0, 0.0, 0.0], 10, Some(filter))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a");
    }

    #[tokio::test]
    async fn delete_by_ids_removes_points() {
        let store = InMemoryVectorStore::new();
        store.ensure_collection("test", 3).await.unwrap();

        let points = vec![VectorPoint {
            id: "a".into(),
            vector: vec![1.0, 0.0, 0.0],
            payload: HashMap::new(),
        }];
        store.upsert("test", points).await.unwrap();
        store.delete_by_ids("test", vec!["a".into()]).await.unwrap();

        let results = store
            .search("test", vec![1.0, 0.0, 0.0], 10, None)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn scroll_all_extracts_strings() {
        let store = InMemoryVectorStore::new();
        store.ensure_collection("test", 3).await.unwrap();

        let points = vec![VectorPoint {
            id: "a".into(),
            vector: vec![1.0, 0.0, 0.0],
            payload: HashMap::from([
                ("name".into(), serde_json::json!("alpha")),
                ("desc".into(), serde_json::json!("first")),
                ("num".into(), serde_json::json!(42)),
            ]),
        }];
        store.upsert("test", points).await.unwrap();

        let result = store.scroll_all("test", "name").await.unwrap();
        assert_eq!(result.len(), 1);
        let fields = result.get("alpha").unwrap();
        assert_eq!(fields.get("desc").unwrap(), "first");
        assert!(!fields.contains_key("num"));
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        assert!((cosine_similarity(&a, &b)).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn default_impl() {
        let store = InMemoryVectorStore::default();
        assert!(!store.collection_exists("any").await.unwrap());
    }

    #[test]
    fn debug_format() {
        let store = InMemoryVectorStore::new();
        let dbg = format!("{store:?}");
        assert!(dbg.contains("InMemoryVectorStore"));
    }
}
