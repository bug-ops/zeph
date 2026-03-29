// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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
}
