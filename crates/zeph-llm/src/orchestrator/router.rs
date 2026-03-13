// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::error::LlmError;

#[cfg(feature = "candle")]
use crate::candle_provider::CandleProvider;
use crate::claude::ClaudeProvider;
use crate::gemini::GeminiProvider;
use crate::ollama::OllamaProvider;
use crate::openai::OpenAiProvider;
use crate::provider::{ChatResponse, ChatStream, LlmProvider, Message, StatusTx, ToolDefinition};

/// Inner provider enum without the Orchestrator variant to break recursive type cycles.
#[derive(Debug, Clone)]
pub enum SubProvider {
    Ollama(OllamaProvider),
    Claude(ClaudeProvider),
    OpenAi(OpenAiProvider),
    Gemini(GeminiProvider),
    #[cfg(feature = "candle")]
    Candle(CandleProvider),
}

impl SubProvider {
    /// Detect and set the context window size from the underlying provider where supported.
    pub async fn auto_detect_context_window(&mut self) {
        if let Self::Ollama(p) = self
            && let Ok(info) = p.fetch_model_info().await
            && let Some(ctx) = info.context_length
        {
            p.set_context_window(ctx);
        }
    }

    pub fn set_status_tx(&mut self, tx: StatusTx) {
        match self {
            Self::Claude(p) => {
                p.status_tx = Some(tx);
            }
            Self::OpenAi(p) => {
                p.status_tx = Some(tx);
            }
            Self::Ollama(_) | Self::Gemini(_) => {}
            #[cfg(feature = "candle")]
            Self::Candle(_) => {}
        }
    }

    /// Fetch available models from the underlying provider.
    ///
    /// # Errors
    ///
    /// Returns an error if the remote request fails.
    pub async fn list_models_remote(
        &self,
    ) -> Result<Vec<crate::model_cache::RemoteModelInfo>, LlmError> {
        match self {
            Self::Ollama(p) => p.list_models_remote().await,
            Self::Claude(p) => p.list_models_remote().await,
            Self::OpenAi(p) => p.list_models_remote().await,
            Self::Gemini(p) => Ok(p
                .list_models()
                .into_iter()
                .map(|id| crate::model_cache::RemoteModelInfo {
                    display_name: id.clone(),
                    id,
                    context_window: None,
                    created_at: None,
                })
                .collect()),
            #[cfg(feature = "candle")]
            Self::Candle(_) => Ok(vec![]),
        }
    }
}

impl LlmProvider for SubProvider {
    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        match self {
            Self::Ollama(p) => p.chat(messages).await,
            Self::Claude(p) => p.chat(messages).await,
            Self::OpenAi(p) => p.chat(messages).await,
            Self::Gemini(p) => p.chat(messages).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.chat(messages).await,
        }
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        match self {
            Self::Ollama(p) => p.chat_stream(messages).await,
            Self::Claude(p) => p.chat_stream(messages).await,
            Self::OpenAi(p) => p.chat_stream(messages).await,
            Self::Gemini(p) => p.chat_stream(messages).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.chat_stream(messages).await,
        }
    }

    fn supports_streaming(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_streaming(),
            Self::Claude(p) => p.supports_streaming(),
            Self::OpenAi(p) => p.supports_streaming(),
            Self::Gemini(p) => p.supports_streaming(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.supports_streaming(),
        }
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        match self {
            Self::Ollama(p) => p.embed(text).await,
            Self::Claude(p) => p.embed(text).await,
            Self::OpenAi(p) => p.embed(text).await,
            Self::Gemini(p) => p.embed(text).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.embed(text).await,
        }
    }

    fn supports_embeddings(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_embeddings(),
            Self::Claude(p) => p.supports_embeddings(),
            Self::OpenAi(p) => p.supports_embeddings(),
            Self::Gemini(p) => p.supports_embeddings(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.supports_embeddings(),
        }
    }

    fn supports_tool_use(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_tool_use(),
            Self::Claude(p) => p.supports_tool_use(),
            Self::OpenAi(p) => p.supports_tool_use(),
            Self::Gemini(p) => p.supports_tool_use(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.supports_tool_use(),
        }
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        match self {
            Self::Ollama(p) => p.chat_with_tools(messages, tools).await,
            Self::Claude(p) => p.chat_with_tools(messages, tools).await,
            Self::OpenAi(p) => p.chat_with_tools(messages, tools).await,
            Self::Gemini(p) => p.chat_with_tools(messages, tools).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.chat_with_tools(messages, tools).await,
        }
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        match self {
            Self::Ollama(p) => p.last_cache_usage(),
            Self::Claude(p) => p.last_cache_usage(),
            Self::OpenAi(p) => p.last_cache_usage(),
            Self::Gemini(p) => p.last_cache_usage(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.last_cache_usage(),
        }
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        match self {
            Self::Ollama(p) => p.name(),
            Self::Claude(p) => p.name(),
            Self::OpenAi(p) => p.name(),
            Self::Gemini(p) => p.name(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.name(),
        }
    }

    fn context_window(&self) -> Option<usize> {
        match self {
            Self::Ollama(p) => p.context_window(),
            Self::Claude(p) => p.context_window(),
            Self::OpenAi(p) => p.context_window(),
            Self::Gemini(p) => p.context_window(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.context_window(),
        }
    }

    fn supports_vision(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_vision(),
            Self::Claude(p) => p.supports_vision(),
            Self::OpenAi(p) => p.supports_vision(),
            Self::Gemini(p) => p.supports_vision(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.supports_vision(),
        }
    }

    fn last_usage(&self) -> Option<(u64, u64)> {
        match self {
            Self::Ollama(p) => p.last_usage(),
            Self::Claude(p) => p.last_usage(),
            Self::OpenAi(p) => p.last_usage(),
            Self::Gemini(p) => p.last_usage(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.last_usage(),
        }
    }

    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        match self {
            Self::Ollama(p) => p.debug_request_json(messages, tools, stream),
            Self::Claude(p) => p.debug_request_json(messages, tools, stream),
            Self::OpenAi(p) => p.debug_request_json(messages, tools, stream),
            Self::Gemini(p) => p.debug_request_json(messages, tools, stream),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.debug_request_json(messages, tools, stream),
        }
    }

    fn supports_structured_output(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_structured_output(),
            Self::Claude(p) => p.supports_structured_output(),
            Self::OpenAi(p) => p.supports_structured_output(),
            Self::Gemini(p) => p.supports_structured_output(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.supports_structured_output(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_provider_ollama_delegates() {
        let sub = SubProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        assert_eq!(sub.name(), "ollama");
        assert!(sub.supports_streaming());
        assert!(sub.supports_embeddings());
    }

    #[test]
    fn sub_provider_claude_delegates() {
        let sub = SubProvider::Claude(ClaudeProvider::new(
            "key".into(),
            "claude-sonnet-4-5-20250929".into(),
            1024,
        ));
        assert_eq!(sub.name(), "claude");
        assert!(sub.supports_streaming());
        assert!(!sub.supports_embeddings());
    }

    #[test]
    fn sub_provider_debug() {
        let sub = SubProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        let debug = format!("{sub:?}");
        assert!(debug.contains("Ollama"));
    }

    #[test]
    fn sub_provider_clone() {
        let sub = SubProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 512));
        let cloned = sub.clone();
        assert_eq!(cloned.name(), sub.name());
    }

    #[test]
    fn sub_provider_openai_delegates() {
        let sub = SubProvider::OpenAi(OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
            1024,
            None,
            None,
        ));
        assert_eq!(sub.name(), "openai");
        assert!(sub.supports_streaming());
        assert!(!sub.supports_embeddings());
        assert!(sub.supports_tool_use());
    }

    #[test]
    fn sub_provider_openai_supports_embeddings_when_embed_model_set() {
        let sub = SubProvider::OpenAi(OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
            1024,
            Some("text-embedding-3-small".into()),
            None,
        ));
        assert!(sub.supports_embeddings());
    }

    #[test]
    fn sub_provider_set_status_tx_does_not_panic_for_ollama() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut sub = SubProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        // Ollama ignores set_status_tx — must not panic
        sub.set_status_tx(tx);
    }

    #[test]
    fn sub_provider_set_status_tx_does_not_panic_for_claude() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut sub = SubProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        sub.set_status_tx(tx);
    }

    #[test]
    fn sub_provider_last_cache_usage_returns_none_for_ollama() {
        let sub = SubProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        assert!(sub.last_cache_usage().is_none());
    }

    #[test]
    fn sub_provider_ollama_does_not_support_tool_use() {
        let sub = SubProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));
        // Ollama does not support structured tool_use in the current implementation
        assert!(!sub.supports_tool_use());
    }

    #[test]
    fn sub_provider_context_window_delegates() {
        let ollama = OllamaProvider::new("http://localhost:11434", "test".into(), "embed".into());
        let expected = ollama.context_window();
        let sub = SubProvider::Ollama(ollama);
        assert_eq!(sub.context_window(), expected);

        let claude = ClaudeProvider::new("key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
        let expected = claude.context_window();
        let sub = SubProvider::Claude(claude);
        assert_eq!(sub.context_window(), expected);
    }

    #[test]
    fn sub_provider_context_window_claude_returns_some() {
        // claude-sonnet model must return Some(200_000), not None
        let sub = SubProvider::Claude(ClaudeProvider::new(
            "key".into(),
            "claude-sonnet-4-5-20250929".into(),
            1024,
        ));
        assert_eq!(sub.context_window(), Some(200_000));
    }

    #[test]
    fn sub_provider_context_window_openai_delegates() {
        // gpt-4o must return Some(128_000) via SubProvider delegation
        let sub = SubProvider::OpenAi(OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
            1024,
            None,
            None,
        ));
        assert_eq!(sub.context_window(), Some(128_000));
    }

    #[test]
    fn sub_provider_context_window_ollama_after_set() {
        // After set_context_window, SubProvider must return Some(...) not None
        let mut ollama =
            OllamaProvider::new("http://localhost:11434", "test".into(), "embed".into());
        ollama.set_context_window(8192);
        let sub = SubProvider::Ollama(ollama);
        assert_eq!(sub.context_window(), Some(8192));
    }

    #[test]
    fn sub_provider_supports_vision_delegates_claude() {
        let sub = SubProvider::Claude(ClaudeProvider::new(
            "key".into(),
            "claude-sonnet-4-5-20250929".into(),
            1024,
        ));
        // Claude provider returns true for vision-capable models
        assert_eq!(sub.supports_vision(), sub.supports_vision());
        // SubProvider must not hard-code false — delegate to inner provider
        let inner = ClaudeProvider::new("key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
        assert_eq!(sub.supports_vision(), inner.supports_vision());
    }

    #[test]
    fn sub_provider_supports_vision_delegates_ollama() {
        let inner = OllamaProvider::new("http://localhost:11434", "test".into(), "embed".into());
        let expected = inner.supports_vision();
        let sub = SubProvider::Ollama(inner);
        assert_eq!(sub.supports_vision(), expected);
    }

    #[test]
    fn sub_provider_last_usage_delegates() {
        let sub = SubProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        // Before any call, last_usage should match the inner provider's value
        let inner = ClaudeProvider::new("key".into(), "model".into(), 1024);
        assert_eq!(sub.last_usage(), inner.last_usage());
    }

    #[test]
    fn sub_provider_supports_structured_output_delegates() {
        let inner = ClaudeProvider::new("key".into(), "model".into(), 1024);
        let expected = inner.supports_structured_output();
        let sub = SubProvider::Claude(inner);
        assert_eq!(sub.supports_structured_output(), expected);

        let inner = OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
            1024,
            None,
            None,
        );
        let expected = inner.supports_structured_output();
        let sub = SubProvider::OpenAi(inner);
        assert_eq!(sub.supports_structured_output(), expected);
    }
}
