#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON parse failed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("rate limited")]
    RateLimited,

    #[error("provider unavailable")]
    Unavailable,

    #[error("empty response from {provider}")]
    EmptyResponse { provider: &'static str },

    #[error("SSE parse error: {0}")]
    SseParse(String),

    #[error("embedding not supported by {provider}")]
    EmbedUnsupported { provider: &'static str },

    #[error("model loading failed: {0}")]
    ModelLoad(String),

    #[error("inference failed: {0}")]
    Inference(String),

    #[error("no route configured")]
    NoRoute,

    #[error("no providers available")]
    NoProviders,

    #[cfg(feature = "candle")]
    #[error("candle error: {0}")]
    Candle(#[from] candle_core::Error),

    #[error("structured output parse failed: {0}")]
    StructuredParse(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, LlmError>;
