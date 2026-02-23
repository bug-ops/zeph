use zeph_core::ChannelError;

#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("transport error: {0}")]
    Transport(String),

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("channel error: {0}")]
    Channel(#[from] ChannelError),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}
