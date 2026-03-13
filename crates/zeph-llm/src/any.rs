// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "candle")]
use crate::candle_provider::CandleProvider;
use crate::claude::ClaudeProvider;
use crate::compatible::CompatibleProvider;
use crate::gemini::GeminiProvider;
use crate::mock::MockProvider;
use crate::ollama::OllamaProvider;
use crate::openai::OpenAiProvider;
use crate::orchestrator::ModelOrchestrator;
#[cfg(feature = "schema")]
use schemars::JsonSchema;
#[cfg(feature = "schema")]
use serde::de::DeserializeOwned;

use crate::provider::{
    ChatResponse, ChatStream, GenerationOverrides, LlmProvider, Message, StatusTx, ToolDefinition,
};
use crate::router::RouterProvider;

/// Generates a match over all `AnyProvider` variants, binding the inner provider
/// and evaluating the given closure for each arm.
macro_rules! delegate_provider {
    ($self:expr, |$p:ident| $expr:expr) => {
        match $self {
            AnyProvider::Ollama($p) => $expr,
            AnyProvider::Claude($p) => $expr,
            AnyProvider::OpenAi($p) => $expr,
            AnyProvider::Gemini($p) => $expr,
            #[cfg(feature = "candle")]
            AnyProvider::Candle($p) => $expr,
            AnyProvider::Compatible($p) => $expr,
            AnyProvider::Orchestrator($p) => $expr,
            AnyProvider::Router($p) => $expr,
            AnyProvider::Mock($p) => $expr,
        }
    };
}

#[derive(Debug, Clone)]
pub enum AnyProvider {
    Ollama(OllamaProvider),
    Claude(ClaudeProvider),
    OpenAi(OpenAiProvider),
    Gemini(GeminiProvider),
    #[cfg(feature = "candle")]
    Candle(CandleProvider),
    Compatible(CompatibleProvider),
    Orchestrator(Box<ModelOrchestrator>),
    Router(Box<RouterProvider>),
    Mock(MockProvider),
}

impl AnyProvider {
    /// Return a cloneable closure that calls `embed()` on this provider.
    pub fn embed_fn(&self) -> impl Fn(&str) -> crate::provider::EmbedFuture + Send + Sync {
        let provider = std::sync::Arc::new(self.clone());
        move |text: &str| -> crate::provider::EmbedFuture {
            let p = std::sync::Arc::clone(&provider);
            let owned = text.to_owned();
            Box::pin(async move { p.embed(&owned).await })
        }
    }

    /// # Errors
    ///
    /// Returns an error if the provider fails or the response cannot be parsed.
    #[cfg(feature = "schema")]
    pub async fn chat_typed_erased<T>(&self, messages: &[Message]) -> Result<T, crate::LlmError>
    where
        T: DeserializeOwned + JsonSchema + 'static,
    {
        delegate_provider!(self, |p| p.chat_typed::<T>(messages).await)
    }

    /// Fetch available models from this provider and update the disk cache.
    ///
    /// Returns an empty list for providers that do not support remote model discovery
    /// (Candle) without returning an error.
    ///
    /// # Errors
    ///
    /// Returns an error if the remote request fails.
    pub async fn list_models_remote(
        &self,
    ) -> Result<Vec<crate::model_cache::RemoteModelInfo>, crate::LlmError> {
        match self {
            AnyProvider::Ollama(p) => p.list_models_remote().await,
            AnyProvider::Claude(p) => p.list_models_remote().await,
            AnyProvider::OpenAi(p) => p.list_models_remote().await,
            AnyProvider::Compatible(p) => p.list_models_remote().await,
            AnyProvider::Gemini(p) => Ok(p
                .list_models()
                .into_iter()
                .map(|id| crate::model_cache::RemoteModelInfo {
                    display_name: id.clone(),
                    id,
                    context_window: None,
                    created_at: None,
                })
                .collect()),
            // Router and Orchestrator use synchronous list_models() to avoid recursive async cycles.
            // Results reflect config-time model lists (potentially stale vs. live remote data).
            AnyProvider::Router(p) => {
                tracing::debug!(
                    "list_models_remote: Router falling back to sync list_models (config-time data)"
                );
                Ok(p.list_models()
                    .into_iter()
                    .map(|id| crate::model_cache::RemoteModelInfo {
                        display_name: id.clone(),
                        id,
                        context_window: None,
                        created_at: None,
                    })
                    .collect())
            }
            AnyProvider::Orchestrator(p) => p.list_models_remote().await,
            #[cfg(feature = "candle")]
            AnyProvider::Candle(_) => Ok(vec![]),
            AnyProvider::Mock(p) => Ok(p.models.clone()),
        }
    }

    /// Persist router state to disk if this provider is a `RouterProvider` with Thompson strategy.
    ///
    /// No-op for all other provider variants.
    pub fn save_router_state(&self) {
        if let Self::Router(p) = self {
            p.save_thompson_state();
        }
    }

    /// Return Thompson Sampling distribution snapshots `(provider, alpha, beta)`.
    ///
    /// Returns an empty vec for non-router providers or EMA strategy.
    #[must_use]
    pub fn router_thompson_stats(&self) -> Vec<(String, f64, f64)> {
        if let Self::Router(p) = self {
            p.thompson_stats()
        } else {
            vec![]
        }
    }

    /// Clone and patch this provider with generation parameter overrides.
    ///
    /// Used by the experiment engine to evaluate each variation with its specific parameters.
    /// `Orchestrator` and `Router` variants are returned unchanged (overrides not supported).
    #[must_use]
    pub fn with_generation_overrides(self, overrides: GenerationOverrides) -> Self {
        match self {
            Self::Ollama(p) => Self::Ollama(p.with_generation_overrides(overrides)),
            Self::Claude(p) => Self::Claude(p.with_generation_overrides(overrides)),
            Self::OpenAi(p) => Self::OpenAi(p.with_generation_overrides(overrides)),
            Self::Gemini(p) => Self::Gemini(p.with_generation_overrides(overrides)),
            Self::Compatible(p) => Self::Compatible(p.with_generation_overrides(overrides)),
            Self::Mock(p) => Self::Mock(p.with_generation_overrides(overrides)),
            #[cfg(feature = "candle")]
            Self::Candle(p) => {
                tracing::warn!("generation overrides not supported for Candle provider");
                Self::Candle(p)
            }
            Self::Orchestrator(_) | Self::Router(_) => {
                tracing::warn!("generation overrides not supported for this provider variant");
                self
            }
        }
    }

    /// Route to a specific named provider (for orchestrators), or fall through to default routing.
    ///
    /// For non-orchestrator providers, the `name` is ignored and regular `chat` is used.
    ///
    /// # Errors
    ///
    /// Returns [`crate::LlmError`] if the underlying provider call fails.
    pub async fn chat_with_named_provider(
        &self,
        name: &str,
        messages: &[Message],
    ) -> Result<String, crate::LlmError> {
        if let Self::Orchestrator(orch) = self {
            return orch.chat_for_named(name, messages).await;
        }
        tracing::debug!(
            name,
            "chat_with_named_provider: not an orchestrator, ignoring model name"
        );
        self.chat(messages).await
    }

    /// Route a tool-aware request to a specific named provider (for orchestrators),
    /// or fall through to `chat_with_tools` with default routing.
    ///
    /// For non-orchestrator providers, the `name` is ignored.
    ///
    /// # Errors
    ///
    /// Returns [`crate::LlmError`] if the underlying provider call fails.
    pub async fn chat_with_named_provider_and_tools(
        &self,
        name: &str,
        messages: &[Message],
        tools: &[crate::provider::ToolDefinition],
    ) -> Result<crate::provider::ChatResponse, crate::LlmError> {
        if let Self::Orchestrator(orch) = self {
            return orch.chat_for_named_with_tools(name, messages, tools).await;
        }
        tracing::debug!(
            name,
            "chat_with_named_provider_and_tools: not an orchestrator, ignoring model name"
        );
        self.chat_with_tools(messages, tools).await
    }

    /// Propagate a status sender to the inner provider (where supported).
    pub fn set_status_tx(&mut self, tx: StatusTx) {
        match self {
            Self::Claude(p) => {
                p.status_tx = Some(tx);
            }
            Self::OpenAi(p) => {
                p.status_tx = Some(tx);
            }
            Self::Compatible(p) => {
                p.set_status_tx(tx);
            }
            Self::Orchestrator(p) => {
                p.set_status_tx(tx);
            }
            Self::Router(p) => {
                p.set_status_tx(tx);
            }
            Self::Gemini(p) => {
                p.set_status_tx(tx);
            }
            Self::Ollama(_) => {}
            #[cfg(feature = "candle")]
            Self::Candle(_) => {}
            Self::Mock(_) => {}
        }
    }
}

impl LlmProvider for AnyProvider {
    fn context_window(&self) -> Option<usize> {
        delegate_provider!(self, |p| p.context_window())
    }

    async fn chat(&self, messages: &[Message]) -> Result<String, crate::LlmError> {
        delegate_provider!(self, |p| p.chat(messages).await)
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, crate::LlmError> {
        delegate_provider!(self, |p| p.chat_stream(messages).await)
    }

    fn supports_streaming(&self) -> bool {
        delegate_provider!(self, |p| p.supports_streaming())
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::LlmError> {
        delegate_provider!(self, |p| p.embed(text).await)
    }

    fn supports_embeddings(&self) -> bool {
        delegate_provider!(self, |p| p.supports_embeddings())
    }

    fn name(&self) -> &str {
        delegate_provider!(self, |p| p.name())
    }

    fn supports_structured_output(&self) -> bool {
        delegate_provider!(self, |p| p.supports_structured_output())
    }

    fn supports_vision(&self) -> bool {
        delegate_provider!(self, |p| p.supports_vision())
    }

    fn supports_tool_use(&self) -> bool {
        delegate_provider!(self, |p| p.supports_tool_use())
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, crate::LlmError> {
        delegate_provider!(self, |p| p.chat_with_tools(messages, tools).await)
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        delegate_provider!(self, |p| p.last_cache_usage())
    }

    fn last_usage(&self) -> Option<(u64, u64)> {
        delegate_provider!(self, |p| p.last_usage())
    }

    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        delegate_provider!(self, |p| p.debug_request_json(messages, tools, stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::ClaudeProvider;
    use crate::ollama::OllamaProvider;
    use crate::provider::MessageMetadata;
    use crate::provider::Role;

    #[test]
    fn any_ollama_context_window_delegates() {
        let mut ollama =
            OllamaProvider::new("http://localhost:11434", "test".into(), "embed".into());
        ollama.set_context_window(8192);
        let provider = AnyProvider::Ollama(ollama);
        assert_eq!(provider.context_window(), Some(8192));
    }

    #[test]
    fn any_claude_context_window_delegates() {
        let provider = AnyProvider::Claude(ClaudeProvider::new(
            "key".into(),
            "claude-sonnet-4-5".into(),
            1024,
        ));
        assert_eq!(provider.context_window(), Some(200_000));
    }

    #[test]
    fn any_ollama_name() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn any_claude_name() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        assert_eq!(provider.name(), "claude");
    }

    #[test]
    fn any_ollama_supports_streaming() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        assert!(provider.supports_streaming());
    }

    #[test]
    fn any_claude_supports_streaming() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        assert!(provider.supports_streaming());
    }

    #[test]
    fn any_ollama_supports_embeddings() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        assert!(provider.supports_embeddings());
    }

    #[test]
    fn any_claude_does_not_support_embeddings() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        assert!(!provider.supports_embeddings());
    }

    #[test]
    fn any_ollama_debug() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        let debug = format!("{provider:?}");
        assert!(debug.contains("Ollama"));
    }

    #[test]
    fn any_claude_debug() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        let debug = format!("{provider:?}");
        assert!(debug.contains("Claude"));
    }

    #[test]
    fn any_ollama_clone() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        let cloned = provider.clone();
        assert_eq!(cloned.name(), "ollama");
    }

    #[test]
    fn any_claude_clone() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        let cloned = provider.clone();
        assert_eq!(cloned.name(), "claude");
    }

    #[tokio::test]
    async fn any_claude_embed_returns_error() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        let result = provider.embed("test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn any_ollama_chat_unreachable_errors() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "test".into(),
            "embed".into(),
        ));
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let result = provider.chat(&messages).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn any_claude_chat_unreachable_errors() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let result = provider.chat(&messages).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn any_ollama_chat_stream_unreachable_errors() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "test".into(),
            "embed".into(),
        ));
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let result = provider.chat_stream(&messages).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn any_claude_chat_stream_unreachable_errors() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let result = provider.chat_stream(&messages).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn any_ollama_embed_unreachable_errors() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "test".into(),
            "embed".into(),
        ));
        let result = provider.embed("test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn any_claude_embed_error_message() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        let result = provider.embed("test").await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("embedding not supported by"));
    }

    #[test]
    fn any_ollama_name_delegates() {
        let inner = OllamaProvider::new("http://127.0.0.1:1", "m".into(), "e".into());
        let any = AnyProvider::Ollama(inner);
        assert_eq!(any.name(), "ollama");
    }

    #[test]
    fn any_claude_name_delegates() {
        let inner = ClaudeProvider::new("k".into(), "m".into(), 1024);
        let any = AnyProvider::Claude(inner);
        assert_eq!(any.name(), "claude");
    }

    #[test]
    fn any_provider_clone_independence() {
        let original = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 2048));
        let cloned = original.clone();
        assert_eq!(original.name(), cloned.name());
        assert!(original.supports_streaming());
        assert!(cloned.supports_streaming());
    }

    #[test]
    fn any_provider_debug_variants() {
        let ollama = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "m".into(),
            "e".into(),
        ));
        let claude = AnyProvider::Claude(ClaudeProvider::new("k".into(), "m".into(), 1024));
        assert!(format!("{ollama:?}").contains("Ollama"));
        assert!(format!("{claude:?}").contains("Claude"));
    }

    #[test]
    fn any_openai_name() {
        let provider = AnyProvider::OpenAi(crate::openai::OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
            1024,
            None,
            None,
        ));
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn any_openai_supports_streaming() {
        let provider = AnyProvider::OpenAi(crate::openai::OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
            1024,
            None,
            None,
        ));
        assert!(provider.supports_streaming());
    }

    #[test]
    fn any_openai_supports_embeddings() {
        let with_embed = AnyProvider::OpenAi(crate::openai::OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
            1024,
            Some("text-embedding-3-small".into()),
            None,
        ));
        assert!(with_embed.supports_embeddings());

        let without_embed = AnyProvider::OpenAi(crate::openai::OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
            1024,
            None,
            None,
        ));
        assert!(!without_embed.supports_embeddings());
    }

    #[test]
    fn any_openai_debug() {
        let provider = AnyProvider::OpenAi(crate::openai::OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
            1024,
            None,
            None,
        ));
        let debug = format!("{provider:?}");
        assert!(debug.contains("OpenAi"));
    }

    #[cfg(feature = "schema")]
    #[tokio::test]
    async fn chat_typed_erased_dispatches_to_mock() {
        #[derive(Debug, serde::Deserialize, schemars::JsonSchema, PartialEq)]
        struct TestOutput {
            value: String,
        }

        let mock =
            crate::mock::MockProvider::with_responses(vec![r#"{"value": "from_mock"}"#.into()]);
        let provider = AnyProvider::Mock(mock);
        let messages = vec![Message::from_legacy(Role::User, "test")];
        let result: TestOutput = provider.chat_typed_erased(&messages).await.unwrap();
        assert_eq!(
            result,
            TestOutput {
                value: "from_mock".into()
            }
        );
    }

    #[test]
    fn any_openai_supports_structured_output() {
        let provider = AnyProvider::OpenAi(crate::openai::OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
            1024,
            None,
            None,
        ));
        assert!(provider.supports_structured_output());
    }

    #[test]
    fn any_ollama_does_not_support_structured_output() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        assert!(!provider.supports_structured_output());
    }

    #[test]
    fn any_claude_supports_vision() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        assert!(provider.supports_vision());
    }

    #[test]
    fn any_openai_supports_vision() {
        let provider = AnyProvider::OpenAi(crate::openai::OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
            1024,
            None,
            None,
        ));
        assert!(provider.supports_vision());
    }

    #[test]
    fn any_ollama_supports_vision() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        assert!(provider.supports_vision());
    }

    #[test]
    fn any_ollama_with_generation_overrides_preserves_variant() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        let overrides = crate::provider::GenerationOverrides {
            temperature: Some(0.3),
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
        };
        let patched = provider.with_generation_overrides(overrides);
        assert!(
            matches!(patched, AnyProvider::Ollama(_)),
            "variant must remain Ollama after with_generation_overrides"
        );
    }
}
