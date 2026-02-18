#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("database error: {0}")]
    Sqlite(#[from] sqlx::Error),

    #[error("Qdrant error: {0}")]
    Qdrant(#[from] Box<qdrant_client::QdrantError>),

    #[error("vector store error: {0}")]
    VectorStore(#[from] crate::vector_store::VectorStoreError),

    #[error("migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    #[error("LLM error: {0}")]
    Llm(#[from] zeph_llm::LlmError),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("integer conversion: {0}")]
    IntConversion(#[from] std::num::TryFromIntError),

    #[error("{0}")]
    Other(String),
}
