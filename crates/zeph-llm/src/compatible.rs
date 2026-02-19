use std::fmt;

use crate::error::LlmError;
use crate::openai::OpenAiProvider;
use crate::provider::{ChatResponse, ChatStream, LlmProvider, Message, StatusTx, ToolDefinition};

pub struct CompatibleProvider {
    inner: OpenAiProvider,
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

impl LlmProvider for CompatibleProvider {
    fn context_window(&self) -> Option<usize> {
        None
    }

    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        self.inner.chat(messages).await
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        self.inner.chat_stream(messages).await
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        self.inner.embed(text).await
    }

    fn supports_embeddings(&self) -> bool {
        self.inner.supports_embeddings()
    }

    fn name(&self) -> &str {
        &self.provider_name
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

    fn supports_tool_use(&self) -> bool {
        self.inner.supports_tool_use()
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
}

impl CompatibleProvider {
    pub fn set_status_tx(&mut self, tx: StatusTx) {
        self.inner.status_tx = Some(tx);
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
    fn supports_tool_use_delegates() {
        assert!(test_provider().supports_tool_use());
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
}
