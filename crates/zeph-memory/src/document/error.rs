#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("unsupported format: {0}")]
    UnsupportedFormat(String),

    #[error("file too large: {0} bytes")]
    FileTooLarge(u64),

    #[cfg(feature = "pdf")]
    #[error("PDF error: {0}")]
    Pdf(String),

    #[error("embedding failed: {0}")]
    Embedding(#[from] zeph_llm::LlmError),

    #[error("storage error: {0}")]
    Storage(#[from] crate::error::MemoryError),
}
