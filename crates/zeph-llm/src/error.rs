// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error type for all LLM provider operations.

/// Errors that can occur in any [`crate::provider::LlmProvider`] operation.
///
/// Use the predicate methods ([`is_rate_limited`](Self::is_rate_limited),
/// [`is_context_length_error`](Self::is_context_length_error),
/// [`is_invalid_input`](Self::is_invalid_input),
/// [`is_beta_header_rejected`](Self::is_beta_header_rejected)) to classify errors
/// before deciding whether to retry, fall back, or propagate.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// Underlying HTTP transport error (connection refused, TLS failure, etc.).
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The API returned a response that could not be decoded as valid JSON.
    #[error("JSON parse failed: {0}")]
    Json(#[from] serde_json::Error),

    /// The provider returned HTTP 429 (too many requests). Callers should back off and retry.
    #[error("rate limited")]
    RateLimited,

    /// The provider is temporarily unavailable (HTTP 5xx or connection error).
    #[error("provider unavailable")]
    Unavailable,

    /// The provider returned a successful HTTP status but no content in the response body.
    #[error("empty response from {provider}")]
    EmptyResponse { provider: String },

    /// A Server-Sent Events frame could not be parsed.
    #[error("SSE parse error: {0}")]
    SseParse(String),

    /// [`crate::provider::LlmProvider::embed`] was called on a provider that does not
    /// support embedding generation.
    #[error("embedding not supported by {provider}")]
    EmbedUnsupported { provider: String },

    /// `Candle` model weights or tokenizer could not be loaded from disk or `HuggingFace` Hub.
    #[error("model loading failed: {0}")]
    ModelLoad(String),

    /// The `Candle` inference worker returned an error or timed out.
    #[error("inference failed: {0}")]
    Inference(String),

    /// The [`crate::router::RouterProvider`] has no providers configured.
    #[error("no route configured")]
    NoRoute,

    /// All providers in a router have been exhausted without a successful response.
    #[error("no providers available")]
    NoProviders,

    /// A Candle tensor operation failed.
    #[cfg(feature = "candle")]
    #[error("candle error: {0}")]
    Candle(#[from] candle_core::Error),

    /// [`crate::provider::LlmProvider::chat_typed`] could not parse the model's response
    /// as the requested type, even after a retry.
    #[error("structured output parse failed: {0}")]
    StructuredParse(String),

    /// The speech-to-text backend rejected the audio or returned an error.
    #[error("transcription failed: {0}")]
    TranscriptionFailed(String),

    /// The prompt exceeds the model's maximum context window. Do not retry with the same input
    /// on another provider — the same input will fail there too. Summarize or truncate first.
    #[error("context length exceeded")]
    ContextLengthExceeded,

    /// The request exceeded the configured per-call timeout.
    #[error("LLM request timed out")]
    Timeout,

    /// A beta header sent in the request was rejected by the API (e.g. `compact-2026-01-12`
    /// deprecated or not yet available). The provider has already disabled the feature
    /// internally; the caller should retry without it.
    #[error("beta header rejected by API: {header}")]
    BetaHeaderRejected { header: String },

    /// The input itself is invalid (HTTP 400). Retrying with the same input on another
    /// provider will not help — the router should break the fallback loop immediately.
    #[error("invalid input for {provider}: {message}")]
    InvalidInput { provider: String, message: String },

    #[error("{0}")]
    Other(String),
}

impl LlmError {
    /// Returns true if this error indicates the context/prompt is too long for the model.
    #[must_use]
    pub fn is_context_length_error(&self) -> bool {
        match self {
            Self::ContextLengthExceeded => true,
            Self::Other(msg) => is_context_length_message(msg),
            _ => false,
        }
    }

    /// Returns true if this error indicates that a beta header was rejected by the API.
    #[must_use]
    pub fn is_beta_header_rejected(&self) -> bool {
        matches!(self, Self::BetaHeaderRejected { .. })
    }

    /// Returns true if this error indicates that the input itself is invalid (HTTP 400).
    ///
    /// Callers (e.g. the router fallback loop) should not retry with a different provider
    /// when this is true — the same input will fail there too.
    #[must_use]
    pub fn is_invalid_input(&self) -> bool {
        matches!(self, Self::InvalidInput { .. })
    }

    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::RateLimited)
    }
}

fn is_context_length_message(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("maximum number of tokens")
        || lower.contains("context length exceeded")
        || lower.contains("maximum context length")
        || lower.contains("context_length_exceeded")
        || lower.contains("prompt is too long")
        || lower.contains("input too long")
}

pub type Result<T> = std::result::Result<T, LlmError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_length_exceeded_variant_is_detected() {
        assert!(LlmError::ContextLengthExceeded.is_context_length_error());
    }

    #[test]
    fn other_with_claude_message_is_detected() {
        let e = LlmError::Other("maximum number of tokens exceeded".into());
        assert!(e.is_context_length_error());
    }

    #[test]
    fn other_with_openai_message_is_detected() {
        let e = LlmError::Other(
            "This model's maximum context length is 4096 tokens. context_length_exceeded".into(),
        );
        assert!(e.is_context_length_error());
    }

    #[test]
    fn other_with_ollama_message_is_detected() {
        let e = LlmError::Other("context length exceeded for model".into());
        assert!(e.is_context_length_error());
    }

    #[test]
    fn unrelated_error_is_not_detected() {
        assert!(!LlmError::Unavailable.is_context_length_error());
        assert!(!LlmError::RateLimited.is_context_length_error());
        assert!(!LlmError::Other("some unrelated error".into()).is_context_length_error());
    }

    #[test]
    fn context_length_exceeded_display() {
        assert_eq!(
            LlmError::ContextLengthExceeded.to_string(),
            "context length exceeded"
        );
    }

    #[test]
    fn beta_header_rejected_is_detected() {
        let e = LlmError::BetaHeaderRejected {
            header: "compact-2026-01-12".into(),
        };
        assert!(e.is_beta_header_rejected());
    }

    #[test]
    fn other_error_is_not_beta_header_rejected() {
        assert!(!LlmError::Unavailable.is_beta_header_rejected());
        assert!(!LlmError::ContextLengthExceeded.is_beta_header_rejected());
        assert!(!LlmError::Other("400 bad request".into()).is_beta_header_rejected());
    }

    #[test]
    fn beta_header_rejected_display() {
        let e = LlmError::BetaHeaderRejected {
            header: "compact-2026-01-12".into(),
        };
        assert!(e.to_string().contains("compact-2026-01-12"));
    }

    #[test]
    fn invalid_input_is_detected() {
        let e = LlmError::InvalidInput {
            provider: "openai".into(),
            message: "maximum sequence length exceeded".into(),
        };
        assert!(e.is_invalid_input());
    }

    #[test]
    fn other_errors_are_not_invalid_input() {
        assert!(!LlmError::Unavailable.is_invalid_input());
        assert!(!LlmError::RateLimited.is_invalid_input());
        assert!(!LlmError::Other("400 bad request".into()).is_invalid_input());
    }

    #[test]
    fn invalid_input_display_includes_provider_and_message() {
        let e = LlmError::InvalidInput {
            provider: "openai".into(),
            message: "input too long".into(),
        };
        let s = e.to_string();
        assert!(s.contains("openai"));
        assert!(s.contains("input too long"));
    }
}
