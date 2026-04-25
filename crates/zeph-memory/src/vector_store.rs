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

/// Error type for [`VectorStore`] operations.
#[derive(Debug, thiserror::Error)]
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
    fn search(
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
