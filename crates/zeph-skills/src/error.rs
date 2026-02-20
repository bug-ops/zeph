#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Qdrant error: {0}")]
    Qdrant(#[from] Box<qdrant_client::QdrantError>),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("integer conversion: {0}")]
    IntConversion(#[from] std::num::TryFromIntError),

    #[error("watcher error: {0}")]
    Watcher(#[from] notify::Error),

    #[error("invalid skill: {0}")]
    Invalid(String),

    #[error("skill not found: {0}")]
    NotFound(String),

    #[error("skill already exists: {0}")]
    AlreadyExists(String),

    #[error("git clone failed: {0}")]
    GitCloneFailed(String),

    #[error("copy failed: {0}")]
    CopyFailed(String),

    #[error("{0}")]
    Other(String),
}
