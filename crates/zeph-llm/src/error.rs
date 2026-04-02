// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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
    EmptyResponse { provider: String },

    #[error("SSE parse error: {0}")]
    SseParse(String),

    #[error("embedding not supported by {provider}")]
    EmbedUnsupported { provider: String },

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

    #[error("transcription failed: {0}")]
    TranscriptionFailed(String),

    #[error("context length exceeded")]
    ContextLengthExceeded,

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
