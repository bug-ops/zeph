use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("failed to bind {0}: {1}")]
    Bind(String, std::io::Error),
    #[error("server error: {0}")]
    Server(String),
}
