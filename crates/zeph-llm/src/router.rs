use crate::any::AnyProvider;
use crate::error::LlmError;
use crate::provider::{ChatResponse, ChatStream, LlmProvider, Message, StatusTx, ToolDefinition};

#[derive(Debug, Clone)]
pub struct RouterProvider {
    providers: Vec<AnyProvider>,
    status_tx: Option<StatusTx>,
}

impl RouterProvider {
    #[must_use]
    pub fn new(providers: Vec<AnyProvider>) -> Self {
        Self {
            providers,
            status_tx: None,
        }
    }

    pub fn set_status_tx(&mut self, tx: StatusTx) {
        for p in &mut self.providers {
            p.set_status_tx(tx.clone());
        }
        self.status_tx = Some(tx);
    }
}

impl LlmProvider for RouterProvider {
    fn context_window(&self) -> Option<usize> {
        self.providers.first().and_then(LlmProvider::context_window)
    }

    fn chat(
        &self,
        messages: &[Message],
    ) -> impl std::future::Future<Output = Result<String, LlmError>> + Send {
        let providers = self.providers.clone();
        let status_tx = self.status_tx.clone();
        let messages = messages.to_vec();
        Box::pin(async move {
            for p in &providers {
                match p.chat(&messages).await {
                    Ok(r) => return Ok(r),
                    Err(e) => {
                        if let Some(ref tx) = status_tx {
                            let _ = tx.send(format!("router: {} failed, falling back", p.name()));
                        }
                        tracing::warn!(provider = p.name(), error = %e, "router fallback");
                    }
                }
            }
            Err(LlmError::NoProviders)
        })
    }

    fn chat_stream(
        &self,
        messages: &[Message],
    ) -> impl std::future::Future<Output = Result<ChatStream, LlmError>> + Send {
        let providers = self.providers.clone();
        let status_tx = self.status_tx.clone();
        let messages = messages.to_vec();
        Box::pin(async move {
            for p in &providers {
                match p.chat_stream(&messages).await {
                    Ok(r) => return Ok(r),
                    Err(e) => {
                        if let Some(ref tx) = status_tx {
                            let _ = tx.send(format!("router: {} failed, falling back", p.name()));
                        }
                        tracing::warn!(provider = p.name(), error = %e, "router stream fallback");
                    }
                }
            }
            Err(LlmError::NoProviders)
        })
    }

    fn supports_streaming(&self) -> bool {
        self.providers.iter().any(LlmProvider::supports_streaming)
    }

    fn embed(
        &self,
        text: &str,
    ) -> impl std::future::Future<Output = Result<Vec<f32>, LlmError>> + Send {
        let providers = self.providers.clone();
        let status_tx = self.status_tx.clone();
        let text = text.to_owned();
        Box::pin(async move {
            for p in &providers {
                if !p.supports_embeddings() {
                    continue;
                }
                match p.embed(&text).await {
                    Ok(r) => return Ok(r),
                    Err(e) => {
                        if let Some(ref tx) = status_tx {
                            let _ =
                                tx.send(format!("router: {} embed failed, falling back", p.name()));
                        }
                        tracing::warn!(provider = p.name(), error = %e, "router embed fallback");
                    }
                }
            }
            Err(LlmError::NoProviders)
        })
    }

    fn supports_embeddings(&self) -> bool {
        self.providers.iter().any(LlmProvider::supports_embeddings)
    }

    fn name(&self) -> &'static str {
        "router"
    }

    fn supports_tool_use(&self) -> bool {
        self.providers.iter().any(LlmProvider::supports_tool_use)
    }

    #[allow(async_fn_in_trait)]
    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        let providers = self.providers.clone();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let status_tx = self.status_tx.clone();
        Box::pin(async move {
            for p in &providers {
                if !p.supports_tool_use() {
                    continue;
                }
                match p.chat_with_tools(&messages, &tools).await {
                    Ok(r) => return Ok(r),
                    Err(e) => {
                        if let Some(ref tx) = status_tx {
                            let _ = tx.send(format!(
                                "router: {} tool call failed, falling back",
                                p.name()
                            ));
                        }
                        tracing::warn!(provider = p.name(), error = %e, "router tool fallback");
                    }
                }
            }
            Err(LlmError::NoProviders)
        })
        .await
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Role;

    #[test]
    fn empty_router_name() {
        let r = RouterProvider::new(vec![]);
        assert_eq!(r.name(), "router");
    }

    #[test]
    fn empty_router_supports_nothing() {
        let r = RouterProvider::new(vec![]);
        assert!(!r.supports_streaming());
        assert!(!r.supports_embeddings());
        assert!(!r.supports_tool_use());
    }

    #[test]
    fn empty_router_context_window_none() {
        let r = RouterProvider::new(vec![]);
        assert!(r.context_window().is_none());
    }

    #[tokio::test]
    async fn empty_router_chat_returns_no_providers() {
        let r = RouterProvider::new(vec![]);
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let err = r.chat(&msgs).await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    #[tokio::test]
    async fn empty_router_chat_stream_returns_no_providers() {
        let r = RouterProvider::new(vec![]);
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let result = r.chat_stream(&msgs).await;
        assert!(matches!(result, Err(LlmError::NoProviders)));
    }

    #[tokio::test]
    async fn empty_router_embed_returns_no_providers() {
        let r = RouterProvider::new(vec![]);
        let err = r.embed("test").await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    #[tokio::test]
    async fn empty_router_chat_with_tools_returns_no_providers() {
        let r = RouterProvider::new(vec![]);
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let err = r.chat_with_tools(&msgs, &[]).await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    #[tokio::test]
    async fn router_falls_back_on_unreachable() {
        use crate::ollama::OllamaProvider;

        let p1 = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "m".into(),
            "e".into(),
        ));
        let p2 = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:2",
            "m".into(),
            "e".into(),
        ));
        let r = RouterProvider::new(vec![p1, p2]);
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let err = r.chat(&msgs).await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    #[test]
    fn router_with_streaming_provider() {
        use crate::ollama::OllamaProvider;

        let p = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "m".into(),
            "e".into(),
        ));
        let r = RouterProvider::new(vec![p]);
        assert!(r.supports_streaming());
        assert!(r.supports_embeddings());
    }

    #[test]
    fn clone_preserves_providers() {
        use crate::ollama::OllamaProvider;

        let p = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "m".into(),
            "e".into(),
        ));
        let r = RouterProvider::new(vec![p]);
        let c = r.clone();
        assert_eq!(c.providers.len(), 1);
        assert_eq!(c.name(), "router");
    }

    #[test]
    fn last_cache_usage_returns_none() {
        let r = RouterProvider::new(vec![]);
        assert!(r.last_cache_usage().is_none());
    }
}
