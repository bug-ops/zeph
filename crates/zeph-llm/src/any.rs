use crate::claude::ClaudeProvider;
use crate::ollama::OllamaProvider;
use crate::provider::{ChatStream, LlmProvider, Message};

#[derive(Debug)]
pub enum AnyProvider {
    Ollama(OllamaProvider),
    Claude(ClaudeProvider),
}

impl LlmProvider for AnyProvider {
    async fn chat(&self, messages: &[Message]) -> anyhow::Result<String> {
        match self {
            Self::Ollama(p) => p.chat(messages).await,
            Self::Claude(p) => p.chat(messages).await,
        }
    }

    async fn chat_stream(&self, messages: &[Message]) -> anyhow::Result<ChatStream> {
        match self {
            Self::Ollama(p) => p.chat_stream(messages).await,
            Self::Claude(p) => p.chat_stream(messages).await,
        }
    }

    fn supports_streaming(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_streaming(),
            Self::Claude(p) => p.supports_streaming(),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Ollama(p) => p.name(),
            Self::Claude(p) => p.name(),
        }
    }
}
