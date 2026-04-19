// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Top-level error type for all `zeph-memory` operations.
///
/// Wraps database, vector-store, LLM, and serialization errors into a single type
/// consumed by callers in `zeph-core`.
///
/// # Examples
///
/// ```rust
/// use zeph_memory::MemoryError;
///
/// fn demo(e: MemoryError) -> String {
///     e.to_string()
/// }
/// ```
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("database error: {0}")]
    Sqlx(#[from] zeph_db::SqlxError),

    #[error("database error: {0}")]
    Db(#[from] zeph_db::DbError),

    #[error("Qdrant error: {0}")]
    Qdrant(#[from] Box<qdrant_client::QdrantError>),

    #[error("vector store error: {0}")]
    VectorStore(#[from] crate::vector_store::VectorStoreError),

    #[error("LLM error: {0}")]
    Llm(#[from] zeph_llm::LlmError),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("integer conversion: {0}")]
    IntConversion(#[from] std::num::TryFromIntError),

    #[error("snapshot error: {0}")]
    Snapshot(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("graph store error: {0}")]
    GraphStore(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("{0}")]
    Other(String),

    #[error("operation timed out: {0}")]
    Timeout(String),

    /// Returned when inserting a supersede pointer would form a cycle in the chain.
    #[error("supersede cycle detected at edge id={0}")]
    SupersedeCycle(i64),

    /// Returned when the supersede chain depth would exceed [`crate::graph::conflict::SUPERSEDE_DEPTH_CAP`].
    #[error("supersede chain depth cap exceeded at edge id={0}")]
    SupersedeDepthExceeded(i64),
}
