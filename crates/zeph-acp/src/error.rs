#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("transport error: {0}")]
    Transport(String),
}
