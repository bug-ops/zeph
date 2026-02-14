use crate::error::LlmError;

#[cfg(feature = "candle")]
use crate::candle_provider::CandleProvider;
use crate::claude::ClaudeProvider;
use crate::ollama::OllamaProvider;
#[cfg(feature = "openai")]
use crate::openai::OpenAiProvider;
use crate::provider::{ChatStream, LlmProvider, Message, StatusTx};

/// Inner provider enum without the Orchestrator variant to break recursive type cycles.
#[derive(Debug, Clone)]
pub enum SubProvider {
    Ollama(OllamaProvider),
    Claude(ClaudeProvider),
    #[cfg(feature = "openai")]
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
            #[cfg(feature = "openai")]
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
            #[cfg(feature = "openai")]
            Self::OpenAi(p) => p.chat(messages).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.chat(messages).await,
        }
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        match self {
            Self::Ollama(p) => p.chat_stream(messages).await,
            Self::Claude(p) => p.chat_stream(messages).await,
            #[cfg(feature = "openai")]
            Self::OpenAi(p) => p.chat_stream(messages).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.chat_stream(messages).await,
        }
    }

    fn supports_streaming(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_streaming(),
            Self::Claude(p) => p.supports_streaming(),
            #[cfg(feature = "openai")]
            Self::OpenAi(p) => p.supports_streaming(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.supports_streaming(),
        }
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        match self {
            Self::Ollama(p) => p.embed(text).await,
            Self::Claude(p) => p.embed(text).await,
            #[cfg(feature = "openai")]
            Self::OpenAi(p) => p.embed(text).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.embed(text).await,
        }
    }

    fn supports_embeddings(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_embeddings(),
            Self::Claude(p) => p.supports_embeddings(),
            #[cfg(feature = "openai")]
            Self::OpenAi(p) => p.supports_embeddings(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.supports_embeddings(),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Ollama(p) => p.name(),
            Self::Claude(p) => p.name(),
            #[cfg(feature = "openai")]
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
}
