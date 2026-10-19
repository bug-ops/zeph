// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error type for all LLM provider operations.

/// Refusal category reported by the provider (for example `cyber` or `bio`).
///
/// The set is open and chosen by the server, so it is kept as an opaque string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusalCategory(String);

impl RefusalCategory {
    /// Wrap a category name reported by the provider.
    ///
    /// The value is server-controlled and shown to the user, so control characters are dropped
    /// and the length is capped at [`Self::MAX_CHARS`].
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(sanitize_provider_text(&name.into(), Self::MAX_CHARS))
    }

    /// Maximum number of characters kept from the provider-supplied category.
    pub const MAX_CHARS: usize = 64;

    /// Category name as reported by the provider.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RefusalCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `stop_details` of a Claude response; populated when `stop_reason` is `refusal`.
#[derive(serde::Deserialize, Debug, Clone, Default)]
pub(crate) struct StopDetails {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub explanation: Option<String>,
}

/// Wire value of `stop_reason` that marks a refusal.
const STOP_REASON_REFUSAL: &str = "refusal";

/// Turn a `refusal` stop reason into [`LlmError::Refused`]; `Ok(())` for every other reason.
///
/// Used as a guard before consuming response content so a declined request surfaces as a typed
/// error instead of an empty reply.
pub(crate) fn check_refusal(
    provider: &str,
    stop_reason: Option<&str>,
    details: Option<&StopDetails>,
) -> Result<()> {
    if stop_reason != Some(STOP_REASON_REFUSAL) {
        return Ok(());
    }
    let details = details.cloned().unwrap_or_default();
    Err(LlmError::Refused {
        provider: provider.to_owned(),
        category: details.category.map(RefusalCategory::new),
        explanation: details.explanation.map(|e| sanitize_provider_text(&e, 500)),
    })
}

/// Drop control characters and cap `text` at `max_chars` characters.
fn sanitize_provider_text(text: &str, max_chars: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect()
}

fn refusal_suffix(category: Option<&RefusalCategory>) -> String {
    category.map_or_else(String::new, |c| format!(" (category: {c})"))
}

/// Errors that can occur in any [`crate::provider::LlmProvider`] operation.
///
/// Use the predicate methods ([`is_rate_limited`](Self::is_rate_limited),
/// [`is_context_length_error`](Self::is_context_length_error),
/// [`is_invalid_input`](Self::is_invalid_input),
/// [`is_model_capability_mismatch`](Self::is_model_capability_mismatch),
/// [`is_beta_header_rejected`](Self::is_beta_header_rejected)) to classify errors
/// before deciding whether to retry, fall back, or propagate.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// Underlying HTTP transport error (connection refused, TLS failure, etc.).
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The API returned a response that could not be decoded as valid JSON.
    #[error("JSON parse failed: {0}")]
    Json(#[from] serde_json::Error),

    /// An I/O error occurred (e.g. reading or writing a cache file).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

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

    /// A thinking block in the request is bound to a different conversation prefix and the
    /// provider rejected it (HTTP 400). Stripping thinking from the history and retrying once
    /// recovers; the same request will fail again unchanged.
    #[error("thinking block bound to a different conversation prefix")]
    ThinkingBindingMismatch,

    /// The input itself is invalid (HTTP 400). Retrying with the same input on another
    /// provider will not help — the router should break the fallback loop immediately.
    #[error("invalid input for {provider}: {message}")]
    InvalidInput { provider: String, message: String },

    /// The model declined to answer (`stop_reason: "refusal"`). The same request will be declined
    /// by the same policy elsewhere or on retry, so routers must not fall back and callers must
    /// surface it instead of retrying.
    #[error("{provider} declined the request{}", refusal_suffix(.category.as_ref()))]
    Refused {
        provider: String,
        category: Option<RefusalCategory>,
        explanation: Option<String>,
    },

    /// The request is well-formed but rejected due to a model- or config-specific
    /// capability gap (e.g. `reasoning_effort` combined with tool calls on `OpenAI`'s Chat
    /// Completions API). Unlike [`Self::InvalidInput`], the same request may succeed on a
    /// different model or provider, so the router should fall back instead of aborting.
    #[error("model capability mismatch for {provider}: {message}")]
    ModelCapabilityMismatch { provider: String, message: String },

    /// A provider returned a non-success HTTP status that does not map to any more specific variant.
    ///
    /// This covers non-retriable API failures such as authentication errors (401/403),
    /// server errors (500/503), and unexpected 4xx responses that are not `InvalidInput`,
    /// `RateLimited`, or `ContextLengthExceeded`. Callers should not retry on this error.
    #[error("{provider} API request failed (status {status})")]
    ApiError { provider: String, status: u16 },

    /// Catch-all for provider-specific errors that do not yet have a typed variant.
    ///
    /// # Deprecation
    ///
    /// Prefer adding a typed variant or propagating a specific source error. This variant
    /// exists for backward compatibility and will be removed once all callsites are migrated.
    #[error("{0}")]
    Other(String),
}

impl LlmError {
    /// Returns true if this error indicates the context/prompt is too long for the model.
    ///
    /// Providers must return [`LlmError::ContextLengthExceeded`] directly; this predicate
    /// does not inspect error message strings.
    #[must_use]
    pub fn is_context_length_error(&self) -> bool {
        matches!(self, Self::ContextLengthExceeded)
    }

    /// Returns true if this error indicates that a beta header was rejected by the API.
    #[must_use]
    pub fn is_beta_header_rejected(&self) -> bool {
        matches!(self, Self::BetaHeaderRejected { .. })
    }

    /// Returns true if the provider rejected a thinking block bound to a different prefix.
    #[must_use]
    pub fn is_thinking_binding_mismatch(&self) -> bool {
        matches!(self, Self::ThinkingBindingMismatch)
    }

    /// Returns true if this error indicates that the input itself is invalid (HTTP 400).
    ///
    /// Callers (e.g. the router fallback loop) should not retry with a different provider
    /// when this is true — the same input will fail there too.
    #[must_use]
    pub fn is_invalid_input(&self) -> bool {
        matches!(self, Self::InvalidInput { .. })
    }

    /// Returns true if the model declined the request.
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        matches!(self, Self::Refused { .. })
    }

    /// Returns true if trying another provider cannot help: the input is invalid or the model
    /// refused it. Routers must stop the fallback loop and record no provider outcome.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::LlmError;
    ///
    /// let refused = LlmError::Refused {
    ///     provider: "claude".into(),
    ///     category: None,
    ///     explanation: None,
    /// };
    /// assert!(refused.forbids_provider_fallback());
    /// assert!(!LlmError::Unavailable.forbids_provider_fallback());
    /// ```
    #[must_use]
    pub fn forbids_provider_fallback(&self) -> bool {
        self.is_invalid_input() || self.is_refusal()
    }

    /// Returns true if this error indicates a model- or config-specific capability gap.
    ///
    /// Unlike [`Self::is_invalid_input`], callers (e.g. the router fallback loop) should
    /// retry with another provider when this is true — the same request may succeed
    /// elsewhere (different model, or without the offending config option).
    #[must_use]
    pub fn is_model_capability_mismatch(&self) -> bool {
        matches!(self, Self::ModelCapabilityMismatch { .. })
    }

    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::RateLimited)
    }
}

/// Check whether a raw API error body text indicates a context-length error.
///
/// Used at the provider transport layer to convert HTTP 400 bodies into
/// [`LlmError::ContextLengthExceeded`] before the error reaches callers.
pub(crate) fn body_is_context_length_error(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("maximum number of tokens")
        || lower.contains("context length exceeded")
        || lower.contains("maximum context length")
        || lower.contains("context_length_exceeded")
        || lower.contains("prompt is too long")
        || lower.contains("input too long")
}

/// Check whether a raw 400 body indicates `OpenAI`'s `reasoning_effort` + `tools`
/// incompatibility on the Chat Completions endpoint (requires `/v1/responses`, which
/// Zeph does not implement).
pub(crate) fn body_is_reasoning_effort_tools_incompatible(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("reasoning_effort")
        && lower.contains("not supported")
        && (lower.contains("/v1/responses") || lower.contains("responses instead"))
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
    fn other_variant_is_not_context_length_error() {
        // The `Other` path no longer triggers context-length classification.
        // Providers must return `ContextLengthExceeded` directly.
        assert!(
            !LlmError::Other("maximum number of tokens exceeded".into()).is_context_length_error()
        );
        assert!(
            !LlmError::Other("context length exceeded for model".into()).is_context_length_error()
        );
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

    #[test]
    fn model_capability_mismatch_is_detected() {
        let e = LlmError::ModelCapabilityMismatch {
            provider: "openai".into(),
            message: "reasoning_effort incompatible with tools".into(),
        };
        assert!(e.is_model_capability_mismatch());
        assert!(!e.is_invalid_input());
    }

    #[test]
    fn other_errors_are_not_model_capability_mismatch() {
        assert!(!LlmError::Unavailable.is_model_capability_mismatch());
        assert!(!LlmError::RateLimited.is_model_capability_mismatch());
        assert!(
            !LlmError::InvalidInput {
                provider: "openai".into(),
                message: "bad request".into(),
            }
            .is_model_capability_mismatch()
        );
    }

    #[test]
    fn model_capability_mismatch_display_includes_provider_and_message() {
        let e = LlmError::ModelCapabilityMismatch {
            provider: "openai".into(),
            message: "reasoning_effort incompatible with tools".into(),
        };
        let s = e.to_string();
        assert!(s.contains("openai"));
        assert!(s.contains("reasoning_effort incompatible with tools"));
    }

    #[test]
    fn api_error_display() {
        let e = LlmError::ApiError {
            provider: "claude".into(),
            status: 503,
        };
        let s = e.to_string();
        assert!(s.contains("claude"));
        assert!(s.contains("503"));
    }

    #[test]
    fn body_is_context_length_error_detects_known_messages() {
        assert!(body_is_context_length_error(
            "maximum number of tokens exceeded"
        ));
        assert!(body_is_context_length_error(
            "This model's maximum context length is 4096 tokens. context_length_exceeded"
        ));
        assert!(body_is_context_length_error(
            "context length exceeded for model"
        ));
        assert!(body_is_context_length_error("prompt is too long"));
        assert!(body_is_context_length_error(
            "input too long for this model"
        ));
    }

    #[test]
    fn body_is_context_length_error_ignores_unrelated_messages() {
        assert!(!body_is_context_length_error("some unrelated error"));
        assert!(!body_is_context_length_error("rate limit exceeded"));
        assert!(!body_is_context_length_error("authentication failed"));
    }

    #[test]
    fn body_is_reasoning_effort_tools_incompatible_detects_known_message() {
        assert!(body_is_reasoning_effort_tools_incompatible(
            "Function tools with reasoning_effort are not supported for gpt-5.4-mini in \
             /v1/chat/completions. Please use /v1/responses instead."
        ));
    }

    #[test]
    fn body_is_reasoning_effort_tools_incompatible_ignores_unrelated_messages() {
        assert!(!body_is_reasoning_effort_tools_incompatible(
            "rate limit exceeded, please retry later"
        ));
        assert!(!body_is_reasoning_effort_tools_incompatible(
            "invalid request: missing required parameter 'model'"
        ));
        assert!(!body_is_reasoning_effort_tools_incompatible(
            "This model's maximum context length is 4096 tokens. context_length_exceeded"
        ));
    }

    #[test]
    fn body_is_reasoning_effort_tools_incompatible_is_case_insensitive() {
        assert!(body_is_reasoning_effort_tools_incompatible(
            "Function tools with REASONING_EFFORT are NOT SUPPORTED for gpt-5.4-mini in \
             /V1/CHAT/COMPLETIONS. Please use /V1/RESPONSES instead."
        ));
    }

    #[test]
    fn body_is_reasoning_effort_tools_incompatible_requires_all_markers() {
        // Mentions reasoning_effort and the responses endpoint, but not "not supported" —
        // should not match, since this isn't necessarily the incompatibility error.
        assert!(!body_is_reasoning_effort_tools_incompatible(
            "reasoning_effort was applied; see /v1/responses for details"
        ));
        // Mentions "not supported" and the responses endpoint, but never reasoning_effort.
        assert!(!body_is_reasoning_effort_tools_incompatible(
            "tool_choice is not supported on /v1/responses for this model"
        ));
        // Mentions reasoning_effort and "not supported", but no responses-endpoint pointer.
        assert!(!body_is_reasoning_effort_tools_incompatible(
            "reasoning_effort is not supported for this model"
        ));
    }

    #[test]
    fn refusal_category_is_sanitized_and_capped() {
        let c = RefusalCategory::new(format!("cy\u{1b}[31mber\n{}", "x".repeat(200)));
        assert!(!c.as_str().chars().any(char::is_control));
        assert_eq!(c.as_str().chars().count(), RefusalCategory::MAX_CHARS);
    }
}
