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
use crate::openai::OpenAiProvider;
use crate::provider::{
    ChatExtras, ChatResponse, ChatStream, GenerationOverrides, LlmProvider, Message, StatusTx,
    ToolDefinition,
};

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
    #[must_use]
    pub fn new(
        provider_name: String,
        api_key: String,
        base_url: String,
        model: String,
        max_tokens: u32,
        embedding_model: Option<String>,
    ) -> Self {
        let inner =
            OpenAiProvider::new(api_key, base_url, model, max_tokens, embedding_model, None);
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
    pub fn set_status_tx(&mut self, tx: StatusTx) {
        self.inner.status_tx = Some(tx);
    }

    #[must_use]
    pub fn with_generation_overrides(mut self, overrides: GenerationOverrides) -> Self {
        self.inner = self.inner.with_generation_overrides(overrides);
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
}

impl LlmProvider for CompatibleProvider {
    fn context_window(&self) -> Option<usize> {
        None
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(
            name = "llm.chat",
            skip_all,
            fields(provider = self.name(), model = self.model_identifier())
        )
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

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(
            name = "llm.chat_stream",
            skip_all,
            fields(provider = self.name(), model = self.model_identifier())
        )
    )]
    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        self.inner.chat_stream(messages).await
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(
            name = "llm.embed",
            skip_all,
            fields(provider = self.name(), model = self.model_identifier())
        )
    )]
    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        self.inner.embed(text).await
    }

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
        CompatibleProvider::new(
            "groq".into(),
            "key".into(),
            "https://api.groq.com/openai/v1".into(),
            "llama-3.3-70b".into(),
            4096,
            None,
        )
    }

    #[test]
    fn name_returns_custom_provider_name() {
        let p = test_provider();
        assert_eq!(p.name(), "groq");
    }

    #[test]
    fn context_window_returns_none() {
        assert!(test_provider().context_window().is_none());
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
        let p = CompatibleProvider::new(
            "test".into(),
            "key".into(),
            "http://localhost".into(),
            "m".into(),
            100,
            Some("embed-model".into()),
        );
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
        let p = CompatibleProvider::new(
            "test".into(),
            "key".into(),
            "http://127.0.0.1:1".into(),
            "m".into(),
            100,
            None,
        );
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
}
