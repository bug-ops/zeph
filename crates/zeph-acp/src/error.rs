#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("transport error: {0}")]
    Transport(String),

    #[error("IDE returned error: {0}")]
    ClientError(String),

    #[error("capability not available: {0}")]
    CapabilityUnavailable(String),

    #[error("channel closed")]
    ChannelClosed,
}
