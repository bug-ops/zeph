#[cfg(feature = "candle")]
use crate::candle_provider::CandleProvider;
use crate::claude::ClaudeProvider;
use crate::compatible::CompatibleProvider;
#[cfg(feature = "mock")]
use crate::mock::MockProvider;
use crate::ollama::OllamaProvider;
use crate::openai::OpenAiProvider;
use crate::orchestrator::ModelOrchestrator;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::provider::{ChatResponse, ChatStream, LlmProvider, Message, StatusTx, ToolDefinition};
use crate::router::RouterProvider;

/// Generates a match over all `AnyProvider` variants, binding the inner provider
/// and evaluating the given closure for each arm.
macro_rules! delegate_provider {
    ($self:expr, |$p:ident| $expr:expr) => {
        match $self {
            AnyProvider::Ollama($p) => $expr,
            AnyProvider::Claude($p) => $expr,
            AnyProvider::OpenAi($p) => $expr,
            #[cfg(feature = "candle")]
            AnyProvider::Candle($p) => $expr,
            AnyProvider::Compatible($p) => $expr,
            AnyProvider::Orchestrator($p) => $expr,
            AnyProvider::Router($p) => $expr,
            #[cfg(feature = "mock")]
            AnyProvider::Mock($p) => $expr,
        }
    };
}

#[derive(Debug, Clone)]
pub enum AnyProvider {
    Ollama(OllamaProvider),
    Claude(ClaudeProvider),
    OpenAi(OpenAiProvider),
    #[cfg(feature = "candle")]
    Candle(CandleProvider),
    Compatible(CompatibleProvider),
    Orchestrator(Box<ModelOrchestrator>),
    Router(Box<RouterProvider>),
    #[cfg(feature = "mock")]
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
    pub async fn chat_typed_erased<T>(&self, messages: &[Message]) -> Result<T, crate::LlmError>
    where
        T: DeserializeOwned + JsonSchema + 'static,
    {
        delegate_provider!(self, |p| p.chat_typed::<T>(messages).await)
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
            Self::Ollama(_) => {}
            #[cfg(feature = "candle")]
            Self::Candle(_) => {}
            #[cfg(feature = "mock")]
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::ClaudeProvider;
    use crate::ollama::OllamaProvider;
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

    #[cfg(feature = "mock")]
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
}
