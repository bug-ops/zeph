// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Abstract vector-store trait and associated types.
//!
//! The [`VectorStore`] trait decouples the rest of `zeph-memory` from any specific
//! vector database. Two implementations ship in this crate:
//!
//! - [`crate::qdrant_ops::QdrantOps`] / [`crate::embedding_store::EmbeddingStore`] —
//!   production Qdrant-backed store.
//! - [`crate::db_vector_store::DbVectorStore`] — `SQLite` BLOB store for testing and offline use.
//! - [`crate::in_memory_store::InMemoryVectorStore`] — purely in-memory store for unit tests.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;

/// Error type for [`VectorStore`] operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VectorStoreError {
    #[error("connection error: {0}")]
    Connection(String),
    #[error("collection error: {0}")]
    Collection(String),
    #[error("upsert error: {0}")]
    Upsert(String),
    #[error("search error: {0}")]
    Search(String),
    #[error("delete error: {0}")]
    Delete(String),
    #[error("scroll error: {0}")]
    Scroll(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    /// Operation is not supported by this backend (e.g. `get_points` on `DbVectorStore`).
    #[error("operation unsupported: {0}")]
    Unsupported(String),
}

/// A vector point to be stored in or retrieved from a [`VectorStore`].
#[derive(Debug, Clone)]
pub struct VectorPoint {
    /// Unique string identifier for the point (e.g. a UUID).
    pub id: String,
    /// Dense embedding vector.
    pub vector: Vec<f32>,
    /// Arbitrary JSON metadata stored alongside the vector.
    pub payload: HashMap<String, serde_json::Value>,
}

/// Filter applied to [`VectorStore::search`] and [`VectorStore::scroll_all`].
///
/// All `must` conditions are `ANDed`; all `must_not` conditions are `ANDed`.
#[derive(Debug, Clone, Default)]
pub struct VectorFilter {
    /// All of these conditions must match.
    pub must: Vec<FieldCondition>,
    /// None of these conditions must match.
    pub must_not: Vec<FieldCondition>,
}

/// A single payload field condition in a [`VectorFilter`].
#[derive(Debug, Clone)]
pub struct FieldCondition {
    /// Payload field name.
    pub field: String,
    /// Expected value for the field.
    pub value: FieldValue,
}

/// Value type in a [`FieldCondition`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum FieldValue {
    /// Exact integer match.
    Integer(i64),
    /// Exact string match.
    Text(String),
}

/// A vector point returned by [`VectorStore::search`] with an attached similarity score.
#[derive(Debug, Clone)]
pub struct ScoredVectorPoint {
    /// Point identifier (matches [`VectorPoint::id`]).
    pub id: String,
    /// Cosine similarity score in `[0, 1]`.
    pub score: f32,
    /// Payload stored alongside the vector.
    pub payload: HashMap<String, serde_json::Value>,
}

/// Shared return type alias for all [`VectorStore`] trait methods.
///
/// Intentionally `pub(crate)` — all [`VectorStore`] implementations are internal to this crate.
/// If the trait is ever made externally extensible, this alias should become `pub`.
pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Result of [`VectorStore::scroll_all`]: maps point ID → key → value payload strings.
pub type ScrollResult = HashMap<String, HashMap<String, String>>;

/// Result of [`VectorStore::scroll_all_with_point_ids`]: a list of `(point_id, string_fields)` pairs.
///
/// Only points whose payload contains `key_field` as a `StringValue` are included.
pub type ScrollWithIdsResult = Vec<(String, HashMap<String, String>)>;

/// Clamp a caller-supplied `search` `limit` to `[1, MAX_SEARCH_LIMIT]` at the
/// [`VectorStore::search`] trait method itself (issue #6616).
///
/// The wrapper methods in `embedding_store`, `embedding_registry`, and `reasoning` already
/// clamp before forwarding to a [`VectorStore`] implementor (issue #6553), but any caller
/// that reaches an implementor directly — e.g. `zeph-index`'s `CodeStore::search` or a
/// generic `V: VectorStore` pipeline step — bypasses those wrappers entirely. The
/// trait-provided [`VectorStore::search`] calls this once before delegating to
/// [`VectorStore::search_clamped`], so the bound holds regardless of the call path.
/// Clamping an already-clamped value is a no-op, so enforcing it at both the wrapper and
/// the trait layer is safe.
fn clamp_search_limit(site: &'static str, limit: u64, warned: &AtomicBool) -> u64 {
    if let Ok(requested) = usize::try_from(limit) {
        crate::warn_if_search_limit_clamped(site, requested, warned);
    }
    limit.clamp(1, crate::MAX_SEARCH_LIMIT as u64)
}

/// Abstraction over a vector database backend.
///
/// Implementations must be `Send + Sync` so they can be wrapped in `Arc` and shared
/// across async tasks. All methods return boxed futures via `BoxFuture` to remain
/// object-safe.
///
/// # Implementations
///
/// | Type | Notes |
/// |------|-------|
/// | [`crate::embedding_store::EmbeddingStore`] | Qdrant-backed; production default. |
/// | [`crate::db_vector_store::DbVectorStore`] | SQLite BLOB; offline / CI use. |
/// | [`crate::in_memory_store::InMemoryVectorStore`] | Fully in-process; unit tests. |
pub trait VectorStore: Send + Sync {
    /// Create a collection with cosine-distance vectors of `vector_size` dimensions.
    ///
    /// Idempotent — no error if the collection already exists with the same dimension.
    fn ensure_collection(
        &self,
        collection: &str,
        vector_size: u64,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>>;

    /// Returns `true` if `collection` exists in the backend.
    fn collection_exists(&self, collection: &str) -> BoxFuture<'_, Result<bool, VectorStoreError>>;

    /// Delete a collection and all its points.
    fn delete_collection(&self, collection: &str) -> BoxFuture<'_, Result<(), VectorStoreError>>;

    /// Upsert `points` into `collection`.
    ///
    /// Points with existing IDs are overwritten; new IDs are inserted.
    fn upsert(
        &self,
        collection: &str,
        points: Vec<VectorPoint>,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>>;

    /// Search `collection` for the `limit` nearest neighbours of `vector`.
    ///
    /// Returns results in descending similarity order.  An optional [`VectorFilter`]
    /// restricts the search space to points matching the payload conditions.
    ///
    /// `limit` is clamped to `[1, MAX_SEARCH_LIMIT]` before delegating to
    /// [`Self::search_clamped`] — this is the sole choke point where the clamp is
    /// enforced, regardless of which implementor handles the call. Implementors MUST
    /// implement [`Self::search_clamped`], not override this method; overriding
    /// `search` bypasses the clamp.
    fn search(
        &self,
        collection: &str,
        vector: Vec<f32>,
        limit: u64,
        filter: Option<VectorFilter>,
    ) -> BoxFuture<'_, Result<Vec<ScoredVectorPoint>, VectorStoreError>> {
        let (site, warned) = self.search_clamp_diagnostics();
        let limit = clamp_search_limit(site, limit, warned);
        self.search_clamped(collection, vector, limit, filter)
    }

    /// Per-implementor diagnostic label and "already warned" flag backing [`Self::search`]'s
    /// one-shot clamp warning.
    ///
    /// [`Self::search`] is one shared default-method body invoked identically for every
    /// implementor, so a `static` declared directly inside it would be a single item shared
    /// by *all* implementors — Rust does not duplicate function-local statics per
    /// monomorphization, and a default method reached through `dyn VectorStore` compiles to
    /// one shared body regardless of the concrete backend behind it. For the same reason, a
    /// generic helper like `std::any::type_name::<Self>()` called from within that one shared
    /// body cannot distinguish implementors either. Each implementor must therefore supply its
    /// own label and flag here — a distinct `&'static str` identifying the concrete type (so an
    /// operator can tell which backend logged the warning) and a reference to a local
    /// `static AtomicBool` initialized to `false` — mirroring the per-call-site static already
    /// used by `EmbeddingStore::search`, `EmbeddingRegistry::search_raw`, and
    /// `ReasoningMemory::search` (see module docs).
    ///
    /// The flag this returns is per-*implementor-type*, not per-instance: every `Self` value
    /// shares the one `static` declared in this method's body. This crate's own test suite
    /// currently has exactly one `logs_contain(...)`-asserting clamp test per implementor type,
    /// which is why that is safe today — a *second* such test against the same concrete type
    /// would silently race on this same flag (the identical #6686 hazard this method exists to
    /// prevent, just reintroduced one level up). If you add another oversized-limit clamp test
    /// for a type that already has one, give the existing test's assertion double duty instead
    /// of adding a second one.
    fn search_clamp_diagnostics(&self) -> (&'static str, &'static AtomicBool);

    /// Backend-specific search implementation invoked by [`Self::search`].
    ///
    /// Do not call directly — call [`Self::search`], which clamps `limit` before
    /// delegating here. Implementors MUST NOT re-clamp `limit`; it is guaranteed to
    /// already be within `[1, MAX_SEARCH_LIMIT]`. Never call `Self::search` from here —
    /// it re-enters this method (infinite recursion).
    fn search_clamped(
        &self,
        collection: &str,
        vector: Vec<f32>,
        limit: u64,
        filter: Option<VectorFilter>,
    ) -> BoxFuture<'_, Result<Vec<ScoredVectorPoint>, VectorStoreError>>;

    /// Delete specific points from `collection` by their string IDs.
    fn delete_by_ids(
        &self,
        collection: &str,
        ids: Vec<String>,
    ) -> BoxFuture<'_, Result<(), VectorStoreError>>;

    /// Scroll (paginate) all points in `collection` and return a map of
    /// `point_id → { key_field → value }` payload entries.
    fn scroll_all(
        &self,
        collection: &str,
        key_field: &str,
    ) -> BoxFuture<'_, Result<ScrollResult, VectorStoreError>>;

    /// Scroll all points in `collection`, returning `(point_id, string_payload_fields)` pairs.
    ///
    /// Only points whose payload contains `key_field` as a string value are included.
    /// Unlike [`Self::scroll_all`], the Qdrant point ID is preserved as the first tuple element
    /// rather than being used as the map key — this is required when consumers need to delete
    /// points by their IDs (e.g. stale-embedding cleanup).
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying scroll operation fails.
    fn scroll_all_with_point_ids(
        &self,
        collection: &str,
        key_field: &str,
    ) -> BoxFuture<'_, Result<ScrollWithIdsResult, VectorStoreError>>;

    /// Return `true` if the backend is reachable and operational.
    fn health_check(&self) -> BoxFuture<'_, Result<bool, VectorStoreError>>;

    /// Create keyword payload indexes for the given field names.
    ///
    /// Default implementation is a no-op (for non-Qdrant backends).
    fn create_keyword_indexes(
        &self,
        _collection: &str,
        _fields: &[&str],
    ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        Box::pin(async { Ok(()) })
    }

    /// Batched vector + payload retrieval by point IDs.
    ///
    /// Returns one [`VectorPoint`] per matched id (missing ids are silently dropped).
    /// Backends that cannot return vectors return `Err(VectorStoreError::Unsupported)`.
    ///
    /// # Errors
    ///
    /// Returns [`VectorStoreError::Unsupported`] when the backend does not support
    /// direct point retrieval with vectors (e.g. `DbVectorStore`, `InMemoryVectorStore`
    /// unless overridden in tests).
    fn get_points(
        &self,
        _collection: &str,
        _ids: Vec<String>,
    ) -> BoxFuture<'_, Result<Vec<VectorPoint>, VectorStoreError>> {
        Box::pin(async {
            Err(VectorStoreError::Unsupported(
                "get_points not implemented for this backend".into(),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Minimal [`VectorStore`] whose `search_clamped` records the `limit` it was given
    /// instead of performing a real search, to prove the trait-provided `search` clamp
    /// is structurally reached regardless of implementor.
    struct RecordingStore {
        last_limit: Arc<AtomicU64>,
    }

    impl VectorStore for RecordingStore {
        fn ensure_collection(
            &self,
            _collection: &str,
            _vector_size: u64,
        ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
            Box::pin(async { Ok(()) })
        }

        fn collection_exists(
            &self,
            _collection: &str,
        ) -> BoxFuture<'_, Result<bool, VectorStoreError>> {
            Box::pin(async { Ok(true) })
        }

        fn delete_collection(
            &self,
            _collection: &str,
        ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
            Box::pin(async { Ok(()) })
        }

        fn upsert(
            &self,
            _collection: &str,
            _points: Vec<VectorPoint>,
        ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
            Box::pin(async { Ok(()) })
        }

        fn search_clamp_diagnostics(&self) -> (&'static str, &'static AtomicBool) {
            static CLAMP_WARNED: AtomicBool = AtomicBool::new(false);
            ("RecordingStore::search", &CLAMP_WARNED)
        }

        fn search_clamped(
            &self,
            _collection: &str,
            _vector: Vec<f32>,
            limit: u64,
            _filter: Option<VectorFilter>,
        ) -> BoxFuture<'_, Result<Vec<ScoredVectorPoint>, VectorStoreError>> {
            self.last_limit.store(limit, Ordering::SeqCst);
            Box::pin(async { Ok(vec![]) })
        }

        fn delete_by_ids(
            &self,
            _collection: &str,
            _ids: Vec<String>,
        ) -> BoxFuture<'_, Result<(), VectorStoreError>> {
            Box::pin(async { Ok(()) })
        }

        fn scroll_all(
            &self,
            _collection: &str,
            _key_field: &str,
        ) -> BoxFuture<'_, Result<ScrollResult, VectorStoreError>> {
            Box::pin(async { Ok(ScrollResult::new()) })
        }

        fn scroll_all_with_point_ids(
            &self,
            _collection: &str,
            _key_field: &str,
        ) -> BoxFuture<'_, Result<ScrollWithIdsResult, VectorStoreError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn health_check(&self) -> BoxFuture<'_, Result<bool, VectorStoreError>> {
            Box::pin(async { Ok(true) })
        }
    }

    #[tokio::test]
    async fn search_clamps_oversized_limit_before_delegating() {
        let last_limit = Arc::new(AtomicU64::new(0));
        let store = RecordingStore {
            last_limit: last_limit.clone(),
        };

        store
            .search("collection", vec![0.0], u64::MAX, None)
            .await
            .unwrap();

        assert_eq!(
            last_limit.load(Ordering::SeqCst),
            crate::MAX_SEARCH_LIMIT as u64
        );
    }

    #[tokio::test]
    async fn search_passes_small_limit_through_unclamped() {
        let last_limit = Arc::new(AtomicU64::new(0));
        let store = RecordingStore {
            last_limit: last_limit.clone(),
        };

        store
            .search("collection", vec![0.0], 5, None)
            .await
            .unwrap();

        assert_eq!(last_limit.load(Ordering::SeqCst), 5);
    }
}
