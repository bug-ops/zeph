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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::ClaudeProvider;
    use crate::ollama::OllamaProvider;
    use crate::provider::Role;

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
        assert!(err.to_string().contains("Claude API does not support"));
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
}
