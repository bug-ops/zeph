//! Error types for zeph-index.

use std::num::TryFromIntError;

/// Errors that can occur during code indexing operations.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// IO error reading source files.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// `SQLite` database error.
    #[error("database error: {0}")]
    Sqlite(#[from] sqlx::Error),

    /// Qdrant vector store error.
    #[error("Qdrant error: {0}")]
    Qdrant(#[from] Box<qdrant_client::QdrantError>),

    /// LLM provider error (embedding).
    #[error("LLM error: {0}")]
    Llm(#[from] zeph_llm::LlmError),

    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Tree-sitter parsing error.
    #[error("parse failed: {0}")]
    Parse(String),

    /// Unsupported or unrecognized language.
    #[error("unsupported language")]
    UnsupportedLanguage,

    /// File watcher error.
    #[error("watcher error: {0}")]
    Watcher(#[from] notify::Error),

    /// Integer conversion error.
    #[error("integer conversion failed: {0}")]
    IntConversion(#[from] TryFromIntError),

    /// Generic catch-all error.
    #[error("{0}")]
    Other(String),
}

/// Result type alias using `IndexError`.
pub type Result<T> = std::result::Result<T, IndexError>;
