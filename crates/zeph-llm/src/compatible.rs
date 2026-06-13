// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `OpenAI`-compatible provider adapter.
//!
//! [`CompatibleProvider`] wraps [`crate::openai::OpenAiProvider`] and adds a named
//! provider label for logging. Use it for any endpoint that exposes the `OpenAI` Chat
//! Completions and Embeddings API (Together AI, Fireworks, Anyscale, local vLLM, etc.).
//!
//! # Configuration
//!
//! ```toml
//! [[llm.providers]]
//! name = "together"
//! type = "compatible"
//! provider_name = "together-ai"
//! base_url = "https://api.together.xyz/v1"
//! model = "meta-llama/Llama-3.3-70B-Instruct-Turbo"
//! max_tokens = 4096
//! api_key_vault = "ZEPH_TOGETHER_API_KEY"
//! ```

use std::fmt;

use crate::error::LlmError;
use crate::openai::{CompletionTokensParam, OpenAiConfig, OpenAiProvider};
use crate::provider::{
    ChatExtras, ChatResponse, ChatStream, GenerationOverrides, LlmProvider, Message, StatusTx,
    ToolDefinition,
};

/// Configuration for [`CompatibleProvider`].
///
/// Pass to [`CompatibleProvider::new`] instead of individual positional arguments to avoid
/// silent parameter transposition.
///
/// # Examples
///
/// ```
/// use zeph_llm::compatible::{CompatibleConfig, CompatibleProvider};
///
/// let cfg = CompatibleConfig {
///     provider_name: "together-ai".into(),
///     api_key: "key".into(),
///     base_url: "https://api.together.xyz/v1".into(),
///     model: "meta-llama/Llama-3.3-70B-Instruct-Turbo".into(),
///     max_tokens: 4096,
///     embedding_model: None,
///     completion_tokens_param: None,
/// };
/// let provider = CompatibleProvider::new(cfg);
/// ```
#[derive(Debug, Clone)]
pub struct CompatibleConfig {
    /// Human-readable provider name used in logs and [`LlmProvider::name`].
    pub provider_name: String,
    /// Secret API key sent in the `Authorization: Bearer` header.
    pub api_key: String,
    /// Base URL of the endpoint, e.g. `"https://api.together.xyz/v1"`.
    pub base_url: String,
    /// Chat model identifier.
    pub model: String,
    /// Upper bound on completion tokens returned by the model.
    pub max_tokens: u32,
    /// Embedding model identifier. Set to `None` when the endpoint does not support embeddings.
    pub embedding_model: Option<String>,
    /// Override which token-limit parameter is used in API requests.
    ///
    /// When `None`, the provider infers the correct field from the model name via the built-in
    /// prefix table. Set explicitly for models the table does not recognise (e.g. fine-tuned
    /// reasoning models whose names do not start with `o` + digit).
    pub completion_tokens_param: Option<CompletionTokensParam>,
}

/// [`LlmProvider`] adapter for OpenAI-compatible REST endpoints.
///
/// Delegates all operations to an inner [`OpenAiProvider`] while exposing a
/// configurable `provider_name` for logging and routing identification.
pub struct CompatibleProvider {
    inner: OpenAiProvider,
    /// Human-readable name used in logs and [`LlmProvider::name`].
    provider_name: String,
}

impl CompatibleProvider {
    /// Create a new provider from a [`CompatibleConfig`].
    #[must_use]
    pub fn new(cfg: CompatibleConfig) -> Self {
        let provider_name = cfg.provider_name;
        let inner = OpenAiProvider::new(OpenAiConfig {
            api_key: cfg.api_key,
            base_url: cfg.base_url,
            model: cfg.model,
            max_tokens: cfg.max_tokens,
            embedding_model: cfg.embedding_model,
            reasoning_effort: None,
            context_window: None,
            completion_tokens_param: cfg.completion_tokens_param,
        });
        Self {
            inner,
            provider_name,
        }
    }
}

impl fmt::Debug for CompatibleProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompatibleProvider")
            .field("provider_name", &self.provider_name)
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl Clone for CompatibleProvider {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            provider_name: self.provider_name.clone(),
        }
    }
}

impl CompatibleProvider {
    /// Fetch models via the inner `OpenAiProvider`. Cache slug is derived from base URL.
    ///
    /// # Errors
    ///
    /// Returns an error if the API request fails.
    pub async fn list_models_remote(
        &self,
    ) -> Result<Vec<crate::model_cache::RemoteModelInfo>, LlmError> {
        self.inner.list_models_remote().await
    }
}

impl CompatibleProvider {
    /// Attach a status channel for streaming progress events to the TUI.
    pub fn set_status_tx(&mut self, tx: StatusTx) {
        self.inner.status_tx = Some(tx);
    }

    /// Override generation parameters (temperature, top-p, etc.) for all subsequent calls.
    #[must_use]
    pub fn with_generation_overrides(mut self, overrides: GenerationOverrides) -> Self {
        self.inner = self.inner.with_generation_overrides(overrides);
        self
    }

    /// Override which token-limit parameter is sent in API requests.
    ///
    /// Delegates to the inner [`OpenAiProvider`]. Use this when the model name is not covered
    /// by the built-in prefix table and the inferred field would produce a 400 error.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::compatible::{CompatibleConfig, CompatibleProvider};
    /// use zeph_llm::openai::CompletionTokensParam;
    ///
    /// let provider = CompatibleProvider::new(CompatibleConfig {
    ///     provider_name: "my-provider".into(),
    ///     api_key: "key".into(),
    ///     base_url: "https://api.example.com/v1".into(),
    ///     model: "my-ft-reasoner-v1".into(),
    ///     max_tokens: 4096,
    ///     embedding_model: None,
    ///     completion_tokens_param: None,
    /// })
    /// .with_completion_tokens_param(CompletionTokensParam::MaxCompletionTokens);
    /// ```
    #[must_use]
    pub fn with_completion_tokens_param(mut self, param: CompletionTokensParam) -> Self {
        self.inner = self.inner.with_completion_tokens_param(param);
        self
    }

    /// Forward MCP tool output schemas as JSON hints appended to tool descriptions.
    ///
    /// Delegates to the inner [`OpenAiProvider`]. When `enabled` is `false` the call is a no-op.
    /// `hint_bytes` caps the JSON representation; `max_description_bytes` caps the combined
    /// description string.
    #[must_use]
    pub fn with_output_schema_forwarding(
        mut self,
        enabled: bool,
        hint_bytes: usize,
        max_description_bytes: usize,
    ) -> Self {
        self.inner =
            self.inner
                .with_output_schema_forwarding(enabled, hint_bytes, max_description_bytes);
        self
    }

    /// Apply a `reasoning_effort` override to the inner [`OpenAiProvider`].
    ///
    /// Delegates to [`OpenAiProvider::set_reasoning_effort`], which validates the value
    /// (`"low"`, `"medium"`, or `"high"`) and logs a warning for any unknown value.
    /// Pass `None` to clear a previously-set effort level.
    pub fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.inner.set_reasoning_effort(effort);
    }
}

impl LlmProvider for CompatibleProvider {
    fn context_window(&self) -> Option<usize> {
        self.inner.context_window()
    }

    #[tracing::instrument(
        name = "llm.chat",
        skip_all,
        fields(provider = self.name(), model = self.model_identifier())
    )]
    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        self.inner.chat(messages).await
    }

    async fn chat_with_extras(
        &self,
        messages: &[Message],
    ) -> Result<(String, ChatExtras), LlmError> {
        self.inner.chat_with_extras(messages).await
    }

    #[tracing::instrument(
        name = "llm.chat_stream",
        skip_all,
        fields(provider = self.name(), model = self.model_identifier())
    )]
    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        self.inner.chat_stream(messages).await
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    #[tracing::instrument(
        name = "llm.embed",
        skip_all,
        fields(provider = self.name(), model = self.model_identifier())
    )]
    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        self.inner.embed(text).await
    }

    #[tracing::instrument(
        name = "llm.embed_batch",
        skip_all,
        fields(provider = self.name(), model = self.model_identifier())
    )]
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, LlmError> {
        self.inner.embed_batch(texts).await
    }

    fn supports_embeddings(&self) -> bool {
        self.inner.supports_embeddings()
    }

    fn name(&self) -> &str {
        &self.provider_name
    }

    fn model_identifier(&self) -> &str {
        self.inner.model_identifier()
    }

    fn list_models(&self) -> Vec<String> {
        self.inner.list_models()
    }

    fn supports_structured_output(&self) -> bool {
        self.inner.supports_structured_output()
    }

    async fn chat_typed<T>(&self, messages: &[Message]) -> Result<T, LlmError>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema + 'static,
        Self: Sized,
    {
        self.inner.chat_typed(messages).await
    }

    #[tracing::instrument(
        name = "llm.chat_with_tools",
        skip_all,
        fields(provider = self.name(), model = self.model_identifier(), tool_count = tools.len())
    )]
    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        self.inner.chat_with_tools(messages, tools).await
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        self.inner.last_cache_usage()
    }

    fn last_usage(&self) -> Option<(u64, u64)> {
        self.inner.last_usage()
    }

    fn last_reasoning_tokens(&self) -> Option<u64> {
        self.inner.last_reasoning_tokens()
    }

    fn supports_vision(&self) -> bool {
        self.inner.supports_vision()
    }

    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        self.inner.debug_request_json(messages, tools, stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_provider() -> CompatibleProvider {
        CompatibleProvider::new(CompatibleConfig {
            provider_name: "groq".into(),
            api_key: "key".into(),
            base_url: "https://api.groq.com/openai/v1".into(),
            model: "llama-3.3-70b".into(),
            max_tokens: 4096,
            embedding_model: None,
            completion_tokens_param: None,
        })
    }

    #[test]
    fn name_returns_custom_provider_name() {
        let p = test_provider();
        assert_eq!(p.name(), "groq");
    }

    #[test]
    fn context_window_delegates_to_inner() {
        // "gpt-4o" is in the prefix table → Some(128_000)
        let p = CompatibleProvider::new(CompatibleConfig {
            provider_name: "openai".into(),
            api_key: "key".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            max_tokens: 4096,
            embedding_model: None,
            completion_tokens_param: None,
        });
        assert_eq!(p.context_window(), Some(128_000));
    }

    #[test]
    fn context_window_unknown_model_returns_some_fallback() {
        // Unknown model falls back to 128_000 default in OpenAiProvider.
        let p = CompatibleProvider::new(CompatibleConfig {
            provider_name: "local".into(),
            api_key: "key".into(),
            base_url: "http://localhost/v1".into(),
            model: "unknown-custom-model".into(),
            max_tokens: 4096,
            embedding_model: None,
            completion_tokens_param: None,
        });
        // OpenAiProvider returns Some(128_000) as fallback for unrecognised models.
        assert!(p.context_window().is_some());
    }

    #[test]
    fn supports_streaming_delegates() {
        assert!(test_provider().supports_streaming());
    }

    #[test]
    fn supports_embeddings_without_model() {
        assert!(!test_provider().supports_embeddings());
    }

    #[test]
    fn supports_embeddings_with_model() {
        let p = CompatibleProvider::new(CompatibleConfig {
            provider_name: "test".into(),
            api_key: "key".into(),
            base_url: "http://localhost".into(),
            model: "m".into(),
            max_tokens: 100,
            embedding_model: Some("embed-model".into()),
            completion_tokens_param: None,
        });
        assert!(p.supports_embeddings());
    }

    #[test]
    fn clone_preserves_name() {
        let p = test_provider();
        let c = p.clone();
        assert_eq!(c.name(), "groq");
    }

    #[test]
    fn debug_contains_provider_name() {
        let debug = format!("{:?}", test_provider());
        assert!(debug.contains("groq"));
        assert!(debug.contains("CompatibleProvider"));
    }

    #[tokio::test]
    async fn chat_unreachable_errors() {
        let p = CompatibleProvider::new(CompatibleConfig {
            provider_name: "test".into(),
            api_key: "key".into(),
            base_url: "http://127.0.0.1:1".into(),
            model: "m".into(),
            max_tokens: 100,
            embedding_model: None,
            completion_tokens_param: None,
        });
        let msgs = vec![Message::from_legacy(crate::provider::Role::User, "hello")];
        assert!(p.chat(&msgs).await.is_err());
    }

    #[tokio::test]
    async fn embed_without_model_errors() {
        let p = test_provider();
        let result = p.embed("test").await;
        assert!(result.is_err());
    }

    #[test]
    fn last_usage_initially_none() {
        assert!(test_provider().last_usage().is_none());
    }

    #[test]
    fn with_output_schema_forwarding_does_not_panic() {
        // Smoke-test that the builder compiles and returns self without panicking.
        let p = test_provider().with_output_schema_forwarding(true, 512, usize::MAX);
        assert_eq!(p.name(), "groq");
    }

    // ── reasoning_effort restore path (#5007 Phase 2) ────────────────────────

    #[test]
    fn set_reasoning_effort_applies_via_compatible() {
        let mut p = test_provider();
        p.set_reasoning_effort(Some("high".into()));
        assert_eq!(p.inner.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn any_provider_set_reasoning_effort_delegates_to_compatible() {
        use crate::any::AnyProvider;
        let mut any = AnyProvider::Compatible(test_provider());
        any.set_reasoning_effort(Some("high".into()));
        let AnyProvider::Compatible(ref p) = any else {
            panic!("variant must remain Compatible");
        };
        assert_eq!(
            p.inner.reasoning_effort.as_deref(),
            Some("high"),
            "Compatible inner OpenAiProvider must have reasoning_effort applied"
        );
    }

    #[test]
    fn supports_vision_delegates_to_inner() {
        // OpenAiProvider always returns true for supports_vision.
        assert!(test_provider().supports_vision());
    }

    #[test]
    fn last_reasoning_tokens_initially_none() {
        assert!(test_provider().last_reasoning_tokens().is_none());
    }
}
