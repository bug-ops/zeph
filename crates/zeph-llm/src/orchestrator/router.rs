use crate::error::LlmError;

#[cfg(feature = "candle")]
use crate::candle_provider::CandleProvider;
use crate::claude::ClaudeProvider;
use crate::ollama::OllamaProvider;
use crate::openai::OpenAiProvider;
use crate::provider::{ChatResponse, ChatStream, LlmProvider, Message, StatusTx, ToolDefinition};

/// Inner provider enum without the Orchestrator variant to break recursive type cycles.
#[derive(Debug, Clone)]
pub enum SubProvider {
    Ollama(OllamaProvider),
    Claude(ClaudeProvider),
    OpenAi(OpenAiProvider),
    #[cfg(feature = "candle")]
    Candle(CandleProvider),
}

impl SubProvider {
    pub fn set_status_tx(&mut self, tx: StatusTx) {
        match self {
            Self::Claude(p) => {
                p.status_tx = Some(tx);
            }
            Self::OpenAi(p) => {
                p.status_tx = Some(tx);
            }
            Self::Ollama(_) => {}
            #[cfg(feature = "candle")]
            Self::Candle(_) => {}
        }
    }
}

impl LlmProvider for SubProvider {
    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        match self {
            Self::Ollama(p) => p.chat(messages).await,
            Self::Claude(p) => p.chat(messages).await,
            Self::OpenAi(p) => p.chat(messages).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.chat(messages).await,
        }
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        match self {
            Self::Ollama(p) => p.chat_stream(messages).await,
            Self::Claude(p) => p.chat_stream(messages).await,
            Self::OpenAi(p) => p.chat_stream(messages).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.chat_stream(messages).await,
        }
    }

    fn supports_streaming(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_streaming(),
            Self::Claude(p) => p.supports_streaming(),
            Self::OpenAi(p) => p.supports_streaming(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.supports_streaming(),
        }
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        match self {
            Self::Ollama(p) => p.embed(text).await,
            Self::Claude(p) => p.embed(text).await,
            Self::OpenAi(p) => p.embed(text).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.embed(text).await,
        }
    }

    fn supports_embeddings(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_embeddings(),
            Self::Claude(p) => p.supports_embeddings(),
            Self::OpenAi(p) => p.supports_embeddings(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.supports_embeddings(),
        }
    }

    fn supports_tool_use(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_tool_use(),
            Self::Claude(p) => p.supports_tool_use(),
            Self::OpenAi(p) => p.supports_tool_use(),
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
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.chat_with_tools(messages, tools).await,
        }
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        match self {
            Self::Ollama(p) => p.last_cache_usage(),
            Self::Claude(p) => p.last_cache_usage(),
            Self::OpenAi(p) => p.last_cache_usage(),
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
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.name(),
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
}
