#[cfg(feature = "candle")]
use crate::candle_provider::CandleProvider;
use crate::claude::ClaudeProvider;
use crate::ollama::OllamaProvider;
#[cfg(feature = "orchestrator")]
use crate::orchestrator::ModelOrchestrator;
use crate::provider::{ChatStream, LlmProvider, Message};

#[derive(Debug, Clone)]
pub enum AnyProvider {
    Ollama(OllamaProvider),
    Claude(ClaudeProvider),
    #[cfg(feature = "candle")]
    Candle(CandleProvider),
    #[cfg(feature = "orchestrator")]
    Orchestrator(Box<ModelOrchestrator>),
}

impl LlmProvider for AnyProvider {
    async fn chat(&self, messages: &[Message]) -> anyhow::Result<String> {
        match self {
            Self::Ollama(p) => p.chat(messages).await,
            Self::Claude(p) => p.chat(messages).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.chat(messages).await,
            #[cfg(feature = "orchestrator")]
            Self::Orchestrator(p) => p.chat(messages).await,
        }
    }

    async fn chat_stream(&self, messages: &[Message]) -> anyhow::Result<ChatStream> {
        match self {
            Self::Ollama(p) => p.chat_stream(messages).await,
            Self::Claude(p) => p.chat_stream(messages).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.chat_stream(messages).await,
            #[cfg(feature = "orchestrator")]
            Self::Orchestrator(p) => p.chat_stream(messages).await,
        }
    }

    fn supports_streaming(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_streaming(),
            Self::Claude(p) => p.supports_streaming(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.supports_streaming(),
            #[cfg(feature = "orchestrator")]
            Self::Orchestrator(p) => p.supports_streaming(),
        }
    }

    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        match self {
            Self::Ollama(p) => p.embed(text).await,
            Self::Claude(p) => p.embed(text).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.embed(text).await,
            #[cfg(feature = "orchestrator")]
            Self::Orchestrator(p) => p.embed(text).await,
        }
    }

    fn supports_embeddings(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_embeddings(),
            Self::Claude(p) => p.supports_embeddings(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.supports_embeddings(),
            #[cfg(feature = "orchestrator")]
            Self::Orchestrator(p) => p.supports_embeddings(),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Ollama(p) => p.name(),
            Self::Claude(p) => p.name(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.name(),
            #[cfg(feature = "orchestrator")]
            Self::Orchestrator(p) => p.name(),
        }
    }
}
