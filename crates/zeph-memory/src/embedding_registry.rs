//! Generic embedding registry backed by Qdrant.
//!
//! Provides deduplication through content-hash delta tracking and collection-level
//! embedding-model change detection.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use qdrant_client::qdrant::{PointStruct, value::Kind};

use crate::QdrantOps;
use crate::vector_store::VectorStoreError;

/// Boxed future returned by an embedding function.
pub type EmbedFuture = Pin<
    Box<dyn Future<Output = Result<Vec<f32>, Box<dyn std::error::Error + Send + Sync>>> + Send>,
>;

/// Trait implemented by domain types that can be stored in an [`EmbeddingRegistry`].
pub trait Embeddable: Send + Sync {
    /// Unique string key used for point-ID generation and delta tracking.
    fn key(&self) -> &str;

    /// blake3 hex hash of all semantically relevant fields.
    fn content_hash(&self) -> String;

    /// Text that will be embedded (e.g. description).
    fn embed_text(&self) -> &str;

    /// Full JSON payload to store in Qdrant. **Must** include a `"key"` field
    /// equal to [`Self::key()`] so [`EmbeddingRegistry`] can recover it on scroll.
    fn to_payload(&self) -> serde_json::Value;
}

/// Counters returned by [`EmbeddingRegistry::sync`].
#[derive(Debug, Default, Clone)]
pub struct SyncStats {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
}

/// Errors produced by [`EmbeddingRegistry`].
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingRegistryError {
    #[error("vector store error: {0}")]
    VectorStore(#[from] VectorStoreError),

    #[error("embedding error: {0}")]
    Embedding(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("dimension probe failed: {0}")]
    DimensionProbe(String),
}

impl From<Box<qdrant_client::QdrantError>> for EmbeddingRegistryError {
    fn from(e: Box<qdrant_client::QdrantError>) -> Self {
        Self::VectorStore(VectorStoreError::Collection(e.to_string()))
    }
}

impl From<serde_json::Error> for EmbeddingRegistryError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

impl From<std::num::TryFromIntError> for EmbeddingRegistryError {
    fn from(e: std::num::TryFromIntError) -> Self {
        Self::DimensionProbe(e.to_string())
    }
}

/// Generic Qdrant-backed embedding registry.
///
/// Owns a [`QdrantOps`] instance, a collection name and a UUID namespace for
/// deterministic point IDs (uuid v5).  The in-memory `hashes` map enables
/// O(1) delta detection between syncs.
pub struct EmbeddingRegistry {
    ops: QdrantOps,
    collection: String,
    namespace: uuid::Uuid,
    hashes: HashMap<String, String>,
}

impl std::fmt::Debug for EmbeddingRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingRegistry")
            .field("collection", &self.collection)
            .finish_non_exhaustive()
    }
}

impl EmbeddingRegistry {
    /// Create a registry wrapping an existing [`QdrantOps`] connection.
    #[must_use]
    pub fn new(ops: QdrantOps, collection: impl Into<String>, namespace: uuid::Uuid) -> Self {
        Self {
            ops,
            collection: collection.into(),
            namespace,
            hashes: HashMap::new(),
        }
    }

    /// Sync `items` into Qdrant, computing a content-hash delta to avoid
    /// unnecessary re-embedding.  Re-creates the collection when the embedding
    /// model changes.
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingRegistryError`] on Qdrant or embedding failures.
    pub async fn sync<T: Embeddable>(
        &mut self,
        items: &[T],
        embedding_model: &str,
        embed_fn: impl Fn(&str) -> EmbedFuture,
    ) -> Result<SyncStats, EmbeddingRegistryError> {
        let mut stats = SyncStats::default();

        self.ensure_collection(&embed_fn).await?;

        let existing = self
            .ops
            .scroll_all(&self.collection, "key")
            .await
            .map_err(|e| {
                EmbeddingRegistryError::VectorStore(VectorStoreError::Scroll(e.to_string()))
            })?;

        let mut current: HashMap<String, (String, &T)> = HashMap::with_capacity(items.len());
        for item in items {
            current.insert(item.key().to_owned(), (item.content_hash(), item));
        }

        let model_changed = existing.values().any(|stored| {
            stored
                .get("embedding_model")
                .is_some_and(|m| m != embedding_model)
        });

        if model_changed {
            tracing::warn!("embedding model changed to '{embedding_model}', recreating collection");
            self.recreate_collection(&embed_fn).await?;
        }

        let mut points_to_upsert = Vec::new();
        for (key, (hash, item)) in &current {
            let needs_update = if let Some(stored) = existing.get(key) {
                model_changed || stored.get("content_hash").is_some_and(|h| h != hash)
            } else {
                true
            };

            if !needs_update {
                stats.unchanged += 1;
                self.hashes.insert(key.clone(), hash.clone());
                continue;
            }

            let vector = match embed_fn(item.embed_text()).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("failed to embed item '{key}': {e:#}");
                    continue;
                }
            };

            let point_id = self.point_id(key);
            let mut payload = item.to_payload();
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "content_hash".into(),
                    serde_json::Value::String(hash.clone()),
                );
                obj.insert(
                    "embedding_model".into(),
                    serde_json::Value::String(embedding_model.to_owned()),
                );
            }
            let payload_map = QdrantOps::json_to_payload(payload)?;

            points_to_upsert.push(PointStruct::new(point_id, vector, payload_map));

            if existing.contains_key(key) {
                stats.updated += 1;
            } else {
                stats.added += 1;
            }
            self.hashes.insert(key.clone(), hash.clone());
        }

        if !points_to_upsert.is_empty() {
            self.ops
                .upsert(&self.collection, points_to_upsert)
                .await
                .map_err(|e| {
                    EmbeddingRegistryError::VectorStore(VectorStoreError::Upsert(e.to_string()))
                })?;
        }

        let orphan_ids: Vec<qdrant_client::qdrant::PointId> = existing
            .keys()
            .filter(|key| !current.contains_key(*key))
            .map(|key| qdrant_client::qdrant::PointId::from(self.point_id(key).as_str()))
            .collect();

        if !orphan_ids.is_empty() {
            stats.removed = orphan_ids.len();
            self.ops
                .delete_by_ids(&self.collection, orphan_ids)
                .await
                .map_err(|e| {
                    EmbeddingRegistryError::VectorStore(VectorStoreError::Delete(e.to_string()))
                })?;
        }

        tracing::info!(
            added = stats.added,
            updated = stats.updated,
            removed = stats.removed,
            unchanged = stats.unchanged,
            collection = &self.collection,
            "embeddings synced"
        );

        Ok(stats)
    }

    /// Search the collection, returning raw scored Qdrant points.
    ///
    /// Consumers map the payloads to their domain types.
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingRegistryError`] if embedding or Qdrant search fails.
    pub async fn search_raw(
        &self,
        query: &str,
        limit: usize,
        embed_fn: impl Fn(&str) -> EmbedFuture,
    ) -> Result<Vec<crate::ScoredVectorPoint>, EmbeddingRegistryError> {
        let query_vec = embed_fn(query)
            .await
            .map_err(|e| EmbeddingRegistryError::Embedding(e.to_string()))?;

        let Ok(limit_u64) = u64::try_from(limit) else {
            return Ok(Vec::new());
        };

        let results = self
            .ops
            .search(&self.collection, query_vec, limit_u64, None)
            .await
            .map_err(|e| {
                EmbeddingRegistryError::VectorStore(VectorStoreError::Search(e.to_string()))
            })?;

        let scored: Vec<crate::ScoredVectorPoint> = results
            .into_iter()
            .map(|point| {
                let payload: HashMap<String, serde_json::Value> = point
                    .payload
                    .into_iter()
                    .filter_map(|(k, v)| {
                        let json_val = match v.kind? {
                            Kind::StringValue(s) => serde_json::Value::String(s),
                            Kind::IntegerValue(i) => serde_json::Value::Number(i.into()),
                            Kind::BoolValue(b) => serde_json::Value::Bool(b),
                            Kind::DoubleValue(d) => {
                                serde_json::Number::from_f64(d).map(serde_json::Value::Number)?
                            }
                            _ => return None,
                        };
                        Some((k, json_val))
                    })
                    .collect();

                let id = match point.id.and_then(|pid| pid.point_id_options) {
                    Some(qdrant_client::qdrant::point_id::PointIdOptions::Uuid(u)) => u,
                    Some(qdrant_client::qdrant::point_id::PointIdOptions::Num(n)) => n.to_string(),
                    None => String::new(),
                };

                crate::ScoredVectorPoint {
                    id,
                    score: point.score,
                    payload,
                }
            })
            .collect();

        Ok(scored)
    }

    fn point_id(&self, key: &str) -> String {
        uuid::Uuid::new_v5(&self.namespace, key.as_bytes()).to_string()
    }

    async fn ensure_collection(
        &self,
        embed_fn: &impl Fn(&str) -> EmbedFuture,
    ) -> Result<(), EmbeddingRegistryError> {
        if self
            .ops
            .collection_exists(&self.collection)
            .await
            .map_err(|e| {
                EmbeddingRegistryError::VectorStore(VectorStoreError::Collection(e.to_string()))
            })?
        {
            return Ok(());
        }

        let probe = embed_fn("dimension probe")
            .await
            .map_err(|e| EmbeddingRegistryError::DimensionProbe(e.to_string()))?;
        let vector_size = u64::try_from(probe.len())?;

        self.ops
            .ensure_collection(&self.collection, vector_size)
            .await
            .map_err(|e| {
                EmbeddingRegistryError::VectorStore(VectorStoreError::Collection(e.to_string()))
            })?;

        tracing::info!(
            collection = &self.collection,
            dimensions = vector_size,
            "created Qdrant collection"
        );

        Ok(())
    }

    async fn recreate_collection(
        &self,
        embed_fn: &impl Fn(&str) -> EmbedFuture,
    ) -> Result<(), EmbeddingRegistryError> {
        if self
            .ops
            .collection_exists(&self.collection)
            .await
            .map_err(|e| {
                EmbeddingRegistryError::VectorStore(VectorStoreError::Collection(e.to_string()))
            })?
        {
            self.ops
                .delete_collection(&self.collection)
                .await
                .map_err(|e| {
                    EmbeddingRegistryError::VectorStore(VectorStoreError::Collection(e.to_string()))
                })?;
            tracing::info!(
                collection = &self.collection,
                "deleted collection for recreation"
            );
        }
        self.ensure_collection(embed_fn).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestItem {
        k: String,
        text: String,
    }

    impl Embeddable for TestItem {
        fn key(&self) -> &str {
            &self.k
        }

        fn content_hash(&self) -> String {
            let mut hasher = blake3::Hasher::new();
            hasher.update(self.text.as_bytes());
            hasher.finalize().to_hex().to_string()
        }

        fn embed_text(&self) -> &str {
            &self.text
        }

        fn to_payload(&self) -> serde_json::Value {
            serde_json::json!({"key": self.k, "text": self.text})
        }
    }

    fn make_item(k: &str, text: &str) -> TestItem {
        TestItem {
            k: k.into(),
            text: text.into(),
        }
    }

    #[test]
    fn registry_new_valid_url() {
        let ops = QdrantOps::new("http://localhost:6334").unwrap();
        let ns = uuid::Uuid::from_bytes([0u8; 16]);
        let reg = EmbeddingRegistry::new(ops, "test_col", ns);
        let dbg = format!("{reg:?}");
        assert!(dbg.contains("EmbeddingRegistry"));
        assert!(dbg.contains("test_col"));
    }

    #[test]
    fn embeddable_content_hash_deterministic() {
        let item = make_item("key", "some text");
        assert_eq!(item.content_hash(), item.content_hash());
    }

    #[test]
    fn embeddable_content_hash_changes() {
        let a = make_item("key", "text a");
        let b = make_item("key", "text b");
        assert_ne!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn embeddable_payload_contains_key() {
        let item = make_item("my-key", "desc");
        let payload = item.to_payload();
        assert_eq!(payload["key"], "my-key");
    }

    #[test]
    fn sync_stats_default() {
        let s = SyncStats::default();
        assert_eq!(s.added, 0);
        assert_eq!(s.updated, 0);
        assert_eq!(s.removed, 0);
        assert_eq!(s.unchanged, 0);
    }

    #[test]
    fn sync_stats_debug() {
        let s = SyncStats {
            added: 1,
            updated: 2,
            removed: 3,
            unchanged: 4,
        };
        let dbg = format!("{s:?}");
        assert!(dbg.contains("added"));
    }

    #[tokio::test]
    async fn search_raw_embed_fail_returns_error() {
        let ops = QdrantOps::new("http://localhost:6334").unwrap();
        let ns = uuid::Uuid::from_bytes([0u8; 16]);
        let reg = EmbeddingRegistry::new(ops, "test", ns);
        let embed_fn = |_: &str| -> EmbedFuture {
            Box::pin(async {
                Err(Box::new(std::io::Error::other("fail"))
                    as Box<dyn std::error::Error + Send + Sync>)
            })
        };
        let result = reg.search_raw("query", 5, embed_fn).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn sync_with_unreachable_qdrant_fails() {
        let ops = QdrantOps::new("http://127.0.0.1:1").unwrap();
        let ns = uuid::Uuid::from_bytes([0u8; 16]);
        let mut reg = EmbeddingRegistry::new(ops, "test", ns);
        let items = vec![make_item("k", "text")];
        let embed_fn = |_: &str| -> EmbedFuture { Box::pin(async { Ok(vec![0.1_f32, 0.2]) }) };
        let result = reg.sync(&items, "model", embed_fn).await;
        assert!(result.is_err());
    }
}
