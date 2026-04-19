// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error types for `zeph-index`.
//!
//! All fallible operations in this crate return [`Result<T>`], which is an alias for
//! `std::result::Result<T, IndexError>`. Each variant wraps the upstream error type with
//! an `#[from]` impl so callers can propagate errors with `?` without manual conversion.

use std::num::TryFromIntError;

/// Errors that can occur during code indexing operations.
///
/// # Examples
///
/// ```
/// use zeph_index::error::{IndexError, Result};
///
/// fn must_succeed() -> Result<()> {
///     Err(IndexError::UnsupportedLanguage)
/// }
///
/// assert!(matches!(must_succeed(), Err(IndexError::UnsupportedLanguage)));
/// ```
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// I/O error reading source files from disk.
    ///
    /// Raised when [`tokio::fs::read_to_string`] or [`std::fs::read_to_string`] fail,
    /// for example because a file was deleted between discovery and indexing.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// `SQLite` database error from `sqlx`.
    ///
    /// Raised by metadata reads/writes in [`crate::store::CodeStore`].
    #[error("database error: {0}")]
    Sqlite(#[from] zeph_db::SqlxError),

    /// Qdrant vector store error.
    ///
    /// Raised by upsert, search, or collection management operations in
    /// [`crate::store::CodeStore`].
    #[error("vector store error: {0}")]
    VectorStore(#[from] zeph_memory::VectorStoreError),

    /// LLM provider error during embedding.
    ///
    /// Raised when the configured embedding provider returns an error, for example
    /// due to a network timeout or an unsupported model name.
    #[error("LLM error: {0}")]
    Llm(#[from] zeph_llm::LlmError),

    /// JSON serialization or deserialization error.
    ///
    /// Raised when Qdrant payload values cannot be serialized or when point
    /// payloads contain unexpected types.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Tree-sitter parsing error.
    ///
    /// Raised when a grammar is unavailable for a language, or when tree-sitter
    /// fails to produce a parse tree (rare — tree-sitter is error-tolerant).
    ///
    /// The inner `String` contains a human-readable description of the failure.
    #[error("parse failed: {0}")]
    Parse(String),

    /// Unsupported or unrecognized language.
    ///
    /// Raised by [`crate::indexer`] when a file has a recognized extension but no
    /// corresponding tree-sitter grammar is available for it.
    #[error("unsupported language")]
    UnsupportedLanguage,

    /// Filesystem watcher initialization error.
    ///
    /// Raised by [`crate::watcher::IndexWatcher::start`] when the underlying
    /// `notify` watcher cannot be created or the root path cannot be watched.
    #[error("watcher error: {0}")]
    Watcher(#[from] notify::Error),

    /// Integer conversion error when mapping `usize` values to `i64` or `u64`.
    ///
    /// Raised when line numbers or chunk counts overflow the target integer type,
    /// which in practice only occurs on pathological inputs.
    #[error("integer conversion failed: {0}")]
    IntConversion(#[from] TryFromIntError),

    /// Embedding call timed out.
    ///
    /// Raised by [`crate::retriever::CodeRetriever`] when `provider.embed()` does not
    /// complete within the configured `embed_timeout_secs`.
    #[error("embedding timed out after {0}s")]
    EmbedTimeout(u64),

    /// Generic catch-all error for cases that do not fit the variants above.
    ///
    /// Used internally for errors like a panicking background thread (e.g., the
    /// directory walk `spawn_blocking` task).
    #[error("{0}")]
    Other(String),
}

/// Result type alias using [`IndexError`].
///
/// Returned by all fallible public functions in `zeph-index`.
///
/// # Examples
///
/// ```
/// use zeph_index::error::{IndexError, Result};
///
/// fn parse_something(ok: bool) -> Result<u32> {
///     if ok { Ok(42) } else { Err(IndexError::UnsupportedLanguage) }
/// }
/// ```
pub type Result<T> = std::result::Result<T, IndexError>;
