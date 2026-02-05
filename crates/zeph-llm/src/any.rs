use crate::claude::ClaudeProvider;
use crate::ollama::OllamaProvider;
use crate::provider::{LlmProvider, Message};

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

    fn name(&self) -> &'static str {
        match self {
            Self::Ollama(p) => p.name(),
            Self::Claude(p) => p.name(),
        }
    }
}
