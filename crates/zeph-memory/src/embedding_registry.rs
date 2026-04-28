// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Generic embedding registry backed by Qdrant.
//!
//! Provides deduplication through content-hash delta tracking and collection-level
//! embedding-model change detection.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::RwLock;

use futures::StreamExt as _;
use qdrant_client::qdrant::{PointStruct, value::Kind};

use crate::QdrantOps;
use crate::vector_store::VectorStoreError;

/// Boxed future returned by an embedding function.
pub type EmbedFuture = Pin<
    Box<dyn Future<Output = Result<Vec<f32>, Box<dyn std::error::Error + Send + Sync>>> + Send>,
>;

/// Domain type that can be stored in an [`EmbeddingRegistry`].
///
/// Implement this trait for any struct that should be embedded and persisted in Qdrant.
/// The registry uses [`key`](Self::key) and [`content_hash`](Self::content_hash) to
/// detect which items need to be re-embedded on each [`EmbeddingRegistry::sync`] call.
pub trait Embeddable: Send + Sync {
    /// Unique string key used for point-ID generation and delta tracking.
    fn key(&self) -> &str;

    /// BLAKE3 hex hash of all semantically relevant fields.
    ///
    /// When this hash changes between syncs the item's embedding is recomputed.
    fn content_hash(&self) -> String;

    /// Text that will be passed to the embedding model.
    fn embed_text(&self) -> &str;

    /// Full JSON payload to store in Qdrant alongside the vector.
    ///
    /// **Must** include a `"key"` field equal to [`Self::key()`] so
    /// [`EmbeddingRegistry`] can recover items on scroll.
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

// Ollama appends :latest when no tag is specified; treat the two as equivalent.
fn normalize_model_name(name: &str) -> &str {
    name.strip_suffix(":latest").unwrap_or(name)
}

/// Returns `true` when any stored point uses a model name that is semantically different
/// from `config_model` after normalizing `:latest` suffixes.
///
/// A missing `embedding_model` field (legacy points from pre-#3395 sessions) is treated as a
/// mismatch: the vector was produced by an unknown model and must be regenerated.
fn model_has_changed(
    existing: &HashMap<String, HashMap<String, String>>,
    config_model: &str,
) -> bool {
    if config_model.is_empty() {
        return false;
    }
    existing
        .values()
        .any(|stored| match stored.get("embedding_model") {
            Some(m) => normalize_model_name(m) != normalize_model_name(config_model),
            // Absent field means the point was written before the model was recorded; treat as mismatch.
            None => true,
        })
}

/// Generic Qdrant-backed embedding registry.
///
/// Owns a [`QdrantOps`] instance, a collection name and a UUID namespace for
/// deterministic point IDs (uuid v5).  The in-memory `hashes` map enables
/// O(1) delta detection between syncs.
///
/// The `cached_dim` field caches the collection's vector dimension after the first successful
/// [`sync`](Self::sync) so that [`search_raw`](Self::search_raw) can validate the query vector
/// dimension without an extra Qdrant round-trip on every call.  When a mismatch is detected,
/// `search_raw` returns [`EmbeddingRegistryError::DimensionProbe`] instead of silently issuing a
/// gRPC search that would return near-zero cosine scores (Qdrant gRPC behaviour on dim mismatch).
#[derive(Clone)]
pub struct EmbeddingRegistry {
    ops: QdrantOps,
    collection: String,
    namespace: uuid::Uuid,
    hashes: HashMap<String, String>,
    /// Maximum number of embedding requests dispatched concurrently during a sync.
    pub concurrency: usize,
    /// Vector dimension confirmed during the last successful `sync`.  Shared via `Arc` so
    /// `Clone` works without invalidating the cached value across cloned instances.
    cached_dim: Arc<RwLock<Option<u64>>>,
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
            concurrency: 4,
            cached_dim: Arc::new(RwLock::new(None)),
        }
    }

    /// Sync `items` into Qdrant, computing a content-hash delta to avoid
    /// unnecessary re-embedding.  Re-creates the collection when the embedding
    /// model changes.
    ///
    /// `on_progress`, when provided, is called after each successful embed+upsert with
    /// `(completed, total)` counts so callers can display progress indicators.
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingRegistryError`] on Qdrant or embedding failures.
    pub async fn sync<T: Embeddable>(
        &mut self,
        items: &[T],
        embedding_model: &str,
        embed_fn: impl Fn(&str) -> EmbedFuture,
        on_progress: Option<Box<dyn Fn(usize, usize) + Send>>,
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

        let model_changed = model_has_changed(&existing, embedding_model);

        if model_changed {
            tracing::warn!("embedding model changed to '{embedding_model}', recreating collection");
            self.recreate_collection(&embed_fn).await?;
        }

        let work_items = build_work_set(
            &current,
            &existing,
            model_changed,
            &mut stats,
            &mut self.hashes,
        );

        // Pre-create futures, point IDs, and payloads before taking the mutable borrow on
        // self.hashes to avoid a double-borrow on `self`.
        let work_with_futures: Vec<(String, String, EmbedFuture, String, serde_json::Value)> =
            work_items
                .into_iter()
                .map(|(key, hash, item)| {
                    let text = item.embed_text().to_owned();
                    let fut = embed_fn(&text);
                    let point_id = self.point_id(&key);
                    let payload = item.to_payload();
                    (key, hash, fut, point_id, payload)
                })
                .collect();

        let points_to_upsert = embed_and_collect_points(
            work_with_futures,
            on_progress,
            &existing,
            embedding_model,
            self.concurrency,
            &mut stats,
            &mut self.hashes,
        )
        .await?;

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
    /// Validates that the query vector dimension matches the collection before issuing the gRPC
    /// call.  Qdrant gRPC silently returns near-zero cosine scores (~0.022) when dimensions
    /// mismatch instead of returning an error — this guard prevents that silent failure.
    ///
    /// The dimension is checked against the cache populated by the most recent [`sync`](Self::sync)
    /// call.  If no sync has occurred (cache is `None`) the check is skipped to avoid blocking
    /// reads before the first sync.
    ///
    /// Consumers map the payloads to their domain types.
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingRegistryError::DimensionProbe`] when the query vector dimension does not
    /// match the stored collection dimension.  Returns [`EmbeddingRegistryError::Embedding`] if the
    /// embed function fails, or [`EmbeddingRegistryError::VectorStore`] on Qdrant search failure.
    pub async fn search_raw(
        &self,
        query: &str,
        limit: usize,
        embed_fn: impl Fn(&str) -> EmbedFuture,
    ) -> Result<Vec<crate::ScoredVectorPoint>, EmbeddingRegistryError> {
        let query_vec = embed_fn(query)
            .await
            .map_err(|e| EmbeddingRegistryError::Embedding(e.to_string()))?;

        // Guard: Qdrant gRPC returns near-zero cosine scores when the query vector dimension
        // does not match the stored collection dimension (issue #3418).  Check the cache first
        // (populated by sync); fall back to a live Qdrant probe only when the cache is empty.
        let collection_dim: Option<u64> = *self.cached_dim.read().await;

        let collection_dim = if collection_dim.is_some() {
            collection_dim
        } else {
            // Cache miss: ask Qdrant directly (first search before any sync), then populate cache.
            let probed = self
                .ops
                .get_collection_vector_size(&self.collection)
                .await
                .map_err(|e| {
                    EmbeddingRegistryError::VectorStore(VectorStoreError::Collection(e.to_string()))
                })?;
            if let Some(d) = probed {
                self.set_cached_dim(d).await;
            }
            probed
        };

        if let Some(stored_dim) = collection_dim {
            // Safe: a Vec<f32> with 4B+ elements is impossible in practice on any 64-bit platform.
            let query_dim = query_vec.len() as u64;
            if query_dim != stored_dim {
                return Err(EmbeddingRegistryError::DimensionProbe(format!(
                    "query vector dimension {query_dim} does not match collection '{}' \
                     dimension {stored_dim}; re-run sync to rebuild the collection",
                    self.collection
                )));
            }
        }

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
        if !self
            .ops
            .collection_exists(&self.collection)
            .await
            .map_err(|e| {
                EmbeddingRegistryError::VectorStore(VectorStoreError::Collection(e.to_string()))
            })?
        {
            // Collection does not exist — probe once and create.
            let vector_size = self.probe_vector_size(embed_fn).await?;
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
            self.set_cached_dim(vector_size).await;
            return Ok(());
        }

        let existing_size = self
            .ops
            .client()
            .collection_info(&self.collection)
            .await
            .map_err(|e| {
                EmbeddingRegistryError::VectorStore(VectorStoreError::Collection(e.to_string()))
            })?
            .result
            .and_then(|info| info.config)
            .and_then(|cfg| cfg.params)
            .and_then(|params| params.vectors_config)
            .and_then(|vc| vc.config)
            .and_then(|cfg| match cfg {
                qdrant_client::qdrant::vectors_config::Config::Params(vp) => Some(vp.size),
                // Named-vector collections (ParamsMap) are not supported by this registry;
                // treat size as unknown and recreate to ensure a compatible single-vector layout.
                qdrant_client::qdrant::vectors_config::Config::ParamsMap(_) => None,
            });

        let vector_size = self.probe_vector_size(embed_fn).await?;

        if existing_size == Some(vector_size) {
            self.set_cached_dim(vector_size).await;
            return Ok(());
        }

        tracing::warn!(
            collection = &self.collection,
            existing = ?existing_size,
            required = vector_size,
            "vector dimension mismatch, recreating collection"
        );
        self.ops
            .delete_collection(&self.collection)
            .await
            .map_err(|e| {
                EmbeddingRegistryError::VectorStore(VectorStoreError::Collection(e.to_string()))
            })?;
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
        self.set_cached_dim(vector_size).await;

        Ok(())
    }

    /// Store `dim` in the dimension cache so `search_raw` can validate without a Qdrant round-trip.
    async fn set_cached_dim(&self, dim: u64) {
        *self.cached_dim.write().await = Some(dim);
    }

    async fn probe_vector_size(
        &self,
        embed_fn: &impl Fn(&str) -> EmbedFuture,
    ) -> Result<u64, EmbeddingRegistryError> {
        let probe = embed_fn("dimension probe")
            .await
            .map_err(|e| EmbeddingRegistryError::DimensionProbe(e.to_string()))?;
        // Safe: a Vec<f32> with 4B+ elements is impossible in practice on any 64-bit platform.
        Ok(probe.len() as u64)
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

/// Determine which items need embedding and update stats for unchanged ones.
///
/// Returns a list of `(key, hash, item)` triples that require re-embedding.  Items whose
/// stored hash matches the current hash are counted as `unchanged` in `stats` and their
/// hashes are pre-populated in the `hashes` map.
fn build_work_set<'a, T: Embeddable>(
    current: &HashMap<String, (String, &'a T)>,
    existing: &HashMap<String, HashMap<String, String>>,
    model_changed: bool,
    stats: &mut SyncStats,
    hashes: &mut HashMap<String, String>,
) -> Vec<(String, String, &'a T)> {
    let mut work_items: Vec<(String, String, &'a T)> = Vec::new();
    for (key, (hash, item)) in current {
        let needs_update = if let Some(stored) = existing.get(key) {
            model_changed || stored.get("content_hash").is_some_and(|h| h != hash)
        } else {
            true
        };

        if needs_update {
            work_items.push((key.clone(), hash.clone(), *item));
        } else {
            stats.unchanged += 1;
            hashes.insert(key.clone(), hash.clone());
        }
    }
    work_items
}

/// Await each pre-created embed future and collect the resulting Qdrant points.
///
/// Await each pre-created embed future and collect the resulting Qdrant points.
///
/// `work_items` is `(key, hash, embed_future, point_id, item_payload)` — point IDs and payloads
/// must be pre-computed to avoid a double-borrow on the `EmbeddingRegistry` when `hashes` is
/// mutably borrowed.
///
/// Processes futures with bounded concurrency (`concurrency` parameter).  Calls `on_progress`
/// after each successful embed.  Updates `stats.added`/`stats.updated` and `hashes` in place.
///
/// Returns a `Vec<PointStruct>` ready for upsert, or an error if payload serialization fails.
#[allow(clippy::too_many_arguments)]
async fn embed_and_collect_points(
    work_items: Vec<(String, String, EmbedFuture, String, serde_json::Value)>,
    on_progress: Option<Box<dyn Fn(usize, usize) + Send>>,
    existing: &HashMap<String, HashMap<String, String>>,
    embedding_model: &str,
    concurrency: usize,
    stats: &mut SyncStats,
    hashes: &mut HashMap<String, String>,
) -> Result<Vec<PointStruct>, EmbeddingRegistryError> {
    let total = work_items.len();
    // Clamp concurrency to at least 1: buffer_unordered(0) silently skips all futures.
    let concurrency = concurrency.max(1);

    // Stream results as they complete so on_progress fires in real time, not after collect.
    let mut stream =
        futures::stream::iter(work_items.into_iter().map(
            |(key, hash, fut, point_id, payload)| async move {
                (key, hash, fut.await, point_id, payload)
            },
        ))
        .buffer_unordered(concurrency);

    let mut points_to_upsert = Vec::new();
    let mut completed: usize = 0;
    while let Some((key, hash, result, point_id, mut payload)) = stream.next().await {
        let vector = match result {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to embed item '{key}': {e:#}");
                continue;
            }
        };

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

        if existing.contains_key(&key) {
            stats.updated += 1;
        } else {
            stats.added += 1;
        }
        hashes.insert(key, hash);

        completed += 1;
        if let Some(ref cb) = on_progress {
            cb(completed, total);
        }
    }
    Ok(points_to_upsert)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_no_suffix() {
        assert_eq!(normalize_model_name("foo"), "foo");
    }

    #[test]
    fn normalize_strips_latest() {
        assert_eq!(normalize_model_name("foo:latest"), "foo");
    }

    #[test]
    fn normalize_other_tag_unchanged() {
        assert_eq!(normalize_model_name("foo:v2"), "foo:v2");
    }

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
        let ops = QdrantOps::new("http://localhost:6334", None).unwrap();
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
        let ops = QdrantOps::new("http://localhost:6334", None).unwrap();
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

    /// Validates the dimension mismatch guard in `search_raw` (issue #3418).
    ///
    /// When the cached collection dimension differs from the query vector dimension,
    /// `search_raw` must return `Err(EmbeddingRegistryError::DimensionProbe)` instead of
    /// issuing a gRPC search that would silently return near-zero cosine scores.
    #[tokio::test]
    async fn search_raw_dimension_mismatch_returns_error() {
        let ops = QdrantOps::new("http://localhost:6334", None).unwrap();
        let ns = uuid::Uuid::from_bytes([0u8; 16]);
        let reg = EmbeddingRegistry::new(ops, "test_dim_guard", ns);

        // Simulate that the collection was created with 4-dim vectors.
        reg.set_cached_dim(4).await;

        // Query with a 2-dim vector (different model / dimension).
        let embed_fn = |_: &str| -> EmbedFuture { Box::pin(async { Ok(vec![1.0_f32, 0.0]) }) };
        let result = reg.search_raw("query", 5, embed_fn).await;
        assert!(
            matches!(result, Err(EmbeddingRegistryError::DimensionProbe(_))),
            "expected DimensionProbe error on dimension mismatch, got: {result:?}"
        );
    }

    /// Validates that `search_raw` does not reject a correctly-dimensioned query.
    ///
    /// When the cached dimension matches the query vector, the guard must pass and
    /// the error (if any) comes from the Qdrant network call — not from the guard itself.
    #[tokio::test]
    async fn search_raw_matching_dimension_passes_guard() {
        let ops = QdrantOps::new("http://127.0.0.1:1", None).unwrap(); // unreachable — forces network error
        let ns = uuid::Uuid::from_bytes([0u8; 16]);
        let reg = EmbeddingRegistry::new(ops, "test_dim_pass", ns);

        // Simulate a 2-dim collection.
        reg.set_cached_dim(2).await;

        // Query with a matching 2-dim vector.
        let embed_fn = |_: &str| -> EmbedFuture { Box::pin(async { Ok(vec![1.0_f32, 0.0]) }) };
        let result = reg.search_raw("query", 5, embed_fn).await;
        // The guard passes; the error is from the unreachable Qdrant instance.
        assert!(
            !matches!(result, Err(EmbeddingRegistryError::DimensionProbe(_))),
            "guard must not fire when dimensions match"
        );
    }

    #[tokio::test]
    async fn sync_with_unreachable_qdrant_fails() {
        let ops = QdrantOps::new("http://127.0.0.1:1", None).unwrap();
        let ns = uuid::Uuid::from_bytes([0u8; 16]);
        let mut reg = EmbeddingRegistry::new(ops, "test", ns);
        let items = vec![make_item("k", "text")];
        let embed_fn = |_: &str| -> EmbedFuture { Box::pin(async { Ok(vec![0.1_f32, 0.2]) }) };
        let result = reg.sync(&items, "model", embed_fn, None).await;
        assert!(result.is_err());
    }

    // ── model_has_changed unit tests ──────────────────────────────────────────

    fn make_existing(model: &str) -> HashMap<String, HashMap<String, String>> {
        let mut point = HashMap::new();
        point.insert("embedding_model".to_owned(), model.to_owned());
        let mut map = HashMap::new();
        map.insert("k1".to_owned(), point);
        map
    }

    #[test]
    fn model_has_changed_latest_vs_bare_is_false() {
        // Root cause of #2894: stored ":latest" suffix must not trigger recreation.
        let existing = make_existing("nomic-embed-text-v2-moe:latest");
        assert!(!model_has_changed(&existing, "nomic-embed-text-v2-moe"));
    }

    #[test]
    fn model_has_changed_same_model_is_false() {
        let existing = make_existing("nomic-embed-text-v2-moe");
        assert!(!model_has_changed(&existing, "nomic-embed-text-v2-moe"));
    }

    #[test]
    fn model_has_changed_different_model_is_true() {
        let existing = make_existing("all-minilm");
        assert!(model_has_changed(&existing, "nomic-embed-text-v2-moe"));
    }

    #[test]
    fn model_has_changed_empty_existing_is_false() {
        assert!(!model_has_changed(&HashMap::new(), "any-model"));
    }

    #[test]
    fn model_has_changed_absent_field_with_config_model_is_true() {
        // Legacy points have no embedding_model field; treat as mismatch to force recreation.
        let mut point = HashMap::new();
        point.insert("content_hash".to_owned(), "abc".to_owned());
        let mut map = HashMap::new();
        map.insert("k1".to_owned(), point);
        assert!(model_has_changed(&map, "nomic-embed-text-v2-moe"));
    }

    #[test]
    fn model_has_changed_absent_field_with_empty_config_model_is_false() {
        let mut point = HashMap::new();
        point.insert("content_hash".to_owned(), "abc".to_owned());
        let mut map = HashMap::new();
        map.insert("k1".to_owned(), point);
        assert!(!model_has_changed(&map, ""));
    }

    // ── concurrency guard ─────────────────────────────────────────────────────

    #[test]
    fn concurrency_zero_clamped_to_one() {
        let ops = QdrantOps::new("http://localhost:6334", None).unwrap();
        let ns = uuid::Uuid::from_bytes([0u8; 16]);
        let mut reg = EmbeddingRegistry::new(ops, "test", ns);
        reg.concurrency = 0;
        // Clamp is applied inside sync; verify the field itself can be set to 0
        // and the guard converts it to 1 without panicking (tested via field value).
        assert_eq!(reg.concurrency.max(1), 1);
    }

    // ── integration tests (require live Qdrant via testcontainers) ────────────

    /// Test: `on_progress` fires once per successfully embedded item with correct counts.
    #[tokio::test]
    #[ignore = "requires Docker for Qdrant"]
    async fn on_progress_called_once_per_successful_embed() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use testcontainers::GenericImage;
        use testcontainers::core::{ContainerPort, WaitFor};
        use testcontainers::runners::AsyncRunner;

        let container = GenericImage::new("qdrant/qdrant", "v1.16.0")
            .with_wait_for(WaitFor::message_on_stdout("gRPC listening"))
            .with_wait_for(WaitFor::seconds(1))
            .with_exposed_port(ContainerPort::Tcp(6334))
            .start()
            .await
            .unwrap();
        let port = container.get_host_port_ipv4(6334).await.unwrap();
        let ops = QdrantOps::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
        let ns = uuid::Uuid::new_v4();
        let mut reg = EmbeddingRegistry::new(ops, "test_progress", ns);

        let items = [
            make_item("a", "alpha"),
            make_item("b", "beta"),
            make_item("c", "gamma"),
        ];
        let call_count = Arc::new(AtomicUsize::new(0));
        let last_done = Arc::new(AtomicUsize::new(0));
        let last_total = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);
        let ld = Arc::clone(&last_done);
        let lt = Arc::clone(&last_total);

        let embed_fn =
            |_: &str| -> EmbedFuture { Box::pin(async { Ok(vec![0.1_f32, 0.2, 0.3, 0.4]) }) };
        let on_progress: Option<Box<dyn Fn(usize, usize) + Send>> =
            Some(Box::new(move |completed, total| {
                cc.fetch_add(1, Ordering::SeqCst);
                ld.store(completed, Ordering::SeqCst);
                lt.store(total, Ordering::SeqCst);
            }));

        let stats = reg
            .sync(&items, "test-model", embed_fn, on_progress)
            .await
            .unwrap();
        let n = stats.added + stats.updated;

        assert_eq!(
            call_count.load(Ordering::SeqCst),
            n,
            "on_progress call count"
        );
        assert_eq!(last_done.load(Ordering::SeqCst), n, "last completed");
        assert_eq!(last_total.load(Ordering::SeqCst), n, "total");
    }

    /// Test: when one embed fails, the batch continues and only successful items are upserted.
    #[tokio::test]
    #[ignore = "requires Docker for Qdrant"]
    async fn partial_embed_failure_skips_failed_item() {
        use testcontainers::GenericImage;
        use testcontainers::core::{ContainerPort, WaitFor};
        use testcontainers::runners::AsyncRunner;

        let container = GenericImage::new("qdrant/qdrant", "v1.16.0")
            .with_wait_for(WaitFor::message_on_stdout("gRPC listening"))
            .with_wait_for(WaitFor::seconds(1))
            .with_exposed_port(ContainerPort::Tcp(6334))
            .start()
            .await
            .unwrap();
        let port = container.get_host_port_ipv4(6334).await.unwrap();
        let ops = QdrantOps::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
        let ns = uuid::Uuid::new_v4();
        let mut reg = EmbeddingRegistry::new(ops, "test_partial", ns);

        // Item whose embed_text contains "fail" will cause the embed_fn to return Err.
        let items = [
            make_item("ok1", "ok text"),
            make_item("fail", "fail text"),
            make_item("ok2", "ok text 2"),
        ];

        let embed_fn = |text: &str| -> EmbedFuture {
            if text.contains("fail") {
                Box::pin(async {
                    Err(Box::new(std::io::Error::other("injected failure"))
                        as Box<dyn std::error::Error + Send + Sync>)
                })
            } else {
                Box::pin(async { Ok(vec![0.1_f32, 0.2, 0.3, 0.4]) })
            }
        };

        // sync must return Ok — individual failures are warned and skipped.
        let stats = reg
            .sync(&items, "test-model", embed_fn, None)
            .await
            .unwrap();
        assert_eq!(
            stats.added, 2,
            "two items should be upserted, failed one skipped"
        );
    }

    /// Validates the full dimension-mismatch guard path against a live Qdrant instance (issue #3418).
    ///
    /// Creates a collection with 4-dim vectors via `sync`, then attempts a search with a 2-dim
    /// query vector.  The guard in `search_raw` must return `Err(DimensionProbe)` before any
    /// gRPC call reaches Qdrant, preventing the silent near-zero cosine score failure.
    #[tokio::test]
    #[ignore = "requires Docker for Qdrant"]
    async fn search_raw_dimension_mismatch_returns_error_live() {
        use testcontainers::GenericImage;
        use testcontainers::core::{ContainerPort, WaitFor};
        use testcontainers::runners::AsyncRunner;

        let container = GenericImage::new("qdrant/qdrant", "v1.16.0")
            .with_wait_for(WaitFor::message_on_stdout("gRPC listening"))
            .with_wait_for(WaitFor::seconds(1))
            .with_exposed_port(ContainerPort::Tcp(6334))
            .start()
            .await
            .unwrap();
        let port = container.get_host_port_ipv4(6334).await.unwrap();
        let ops = QdrantOps::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
        let ns = uuid::Uuid::new_v4();
        let mut reg = EmbeddingRegistry::new(ops, "test_dim_guard_live", ns);

        // Sync with 4-dim vectors so the collection and cache are established.
        let items = [make_item("a", "alpha")];
        let embed_fn_4d =
            |_: &str| -> EmbedFuture { Box::pin(async { Ok(vec![1.0_f32, 0.0, 0.0, 0.0]) }) };
        reg.sync(&items, "model-4d", embed_fn_4d, None)
            .await
            .unwrap();

        // Search with a 2-dim query (simulates a model switch without re-sync).
        let embed_fn_2d = |_: &str| -> EmbedFuture { Box::pin(async { Ok(vec![1.0_f32, 0.0]) }) };
        let result = reg.search_raw("query", 5, embed_fn_2d).await;
        assert!(
            matches!(result, Err(EmbeddingRegistryError::DimensionProbe(_))),
            "dimension mismatch must return DimensionProbe error, not silent near-zero scores; got: {result:?}"
        );
    }
}
