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
#[non_exhaustive]
pub enum MemoryError {
    #[error("sqlx error: {0}")]
    Sqlx(#[from] zeph_db::SqlxError),

    #[error("db error: {0}")]
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

    /// A mutex or `RwLock` was poisoned by a panicking thread.
    ///
    /// This indicates a programming error (a thread panicked while holding the lock).
    /// The inner string describes which lock was poisoned.
    #[error("lock poisoned: {0}")]
    LockPoisoned(String),

    /// Catch-all for errors that do not yet have a specific typed variant.
    ///
    /// # Deprecation
    ///
    /// Prefer adding a typed variant over using `Other`. This variant exists for
    /// backward compatibility and will be removed once all callsites are migrated.
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

    /// A promotion-scan or promote error (Feature A, #3305).
    ///
    /// Wraps errors from clustering, skill generation, evaluator calls, or disk writes.
    #[error("promotion scan failed: {0}")]
    Promotion(String),

    /// An error during `zeph knowledge ingest` (spec-067).
    ///
    /// Covers path-validation failures, unsupported source kinds, and per-file ingest errors
    /// reported by the notes-sink pipeline.
    #[error("ingest error: {0}")]
    Ingest(String),

    /// The post-extract validator rejected this extraction result.
    ///
    /// Returned by `extract_and_store` when the `post_extract_validator` callback returns
    /// `Err`. The caller (`ingest_documents`) converts this into `DocOutcome::Rejected`
    /// so the document is counted separately from hard failures.
    #[error("validation rejected: {0}")]
    ValidationRejected(String),
}

impl MemoryError {
    /// Returns `true` when this error is a database foreign-key constraint violation.
    ///
    /// Used to distinguish silent-drop-worthy edge/entity write failures (e.g. a stale
    /// cross-database entity id) from ordinary transient DB errors, so callers can log the
    /// former at `WARN` instead of `DEBUG` (#5801).
    #[must_use]
    pub fn is_foreign_key_violation(&self) -> bool {
        use sqlx::error::DatabaseError;

        matches!(self, Self::Sqlx(err) if err
            .as_database_error()
            .is_some_and(DatabaseError::is_foreign_key_violation))
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryError;

    #[test]
    fn sqlx_variant_display() {
        let inner = zeph_db::SqlxError::RowNotFound;
        let err = MemoryError::Sqlx(inner);
        assert!(
            err.to_string().starts_with("sqlx error:"),
            "unexpected display: {err}"
        );
    }

    #[test]
    fn db_variant_display() {
        let inner = zeph_db::DbError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "test"));
        let err = MemoryError::Db(inner);
        assert!(
            err.to_string().starts_with("db error:"),
            "unexpected display: {err}"
        );
    }

    #[test]
    fn is_foreign_key_violation_false_for_non_constraint_error() {
        let err = MemoryError::Sqlx(zeph_db::SqlxError::RowNotFound);
        assert!(!err.is_foreign_key_violation());
    }

    #[test]
    fn is_foreign_key_violation_false_for_non_sqlx_variant() {
        let err = MemoryError::GraphStore("unrelated failure".into());
        assert!(!err.is_foreign_key_violation());
    }

    /// Regression test for #5801: `is_foreign_key_violation()` must recognize a genuine
    /// FK constraint violation (not just a manufactured/mocked one), since it is the
    /// linchpin of the WARN-vs-DEBUG observability fix.
    #[tokio::test]
    async fn is_foreign_key_violation_true_for_real_fk_violation() {
        use crate::graph::GraphStore;
        use crate::store::SqliteStore;

        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let store = GraphStore::new(sqlite.pool().clone());

        // Neither entity id exists in this fresh database — a genuine FK violation.
        let err = store
            .insert_edge(999_999, 888_888, "related_to", "fact", 0.9, None, None)
            .await
            .expect_err("inserting an edge between nonexistent entities must fail");

        assert!(
            err.is_foreign_key_violation(),
            "expected a foreign-key violation, got: {err:#}"
        );
    }
}
