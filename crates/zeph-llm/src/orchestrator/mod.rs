mod classifier;
mod router;

pub use classifier::TaskType;
pub use router::SubProvider;

use std::collections::HashMap;

use crate::error::LlmError;
use crate::provider::{ChatResponse, ChatStream, LlmProvider, Message, StatusTx, ToolDefinition};

#[derive(Debug, Clone)]
pub struct ModelOrchestrator {
    routes: HashMap<TaskType, Vec<String>>,
    providers: HashMap<String, SubProvider>,
    default_provider: String,
    embed_provider: String,
    status_tx: Option<StatusTx>,
}

impl ModelOrchestrator {
    /// Create a new `ModelOrchestrator`.
    ///
    /// # Errors
    ///
    /// Returns an error if the default or embed provider is not found.
    pub fn new(
        routes: HashMap<TaskType, Vec<String>>,
        providers: HashMap<String, SubProvider>,
        default_provider: String,
        embed_provider: String,
    ) -> Result<Self, LlmError> {
        if !providers.contains_key(&default_provider) {
            return Err(LlmError::Other(format!(
                "default provider '{default_provider}' not found in providers"
            )));
        }
        if !providers.contains_key(&embed_provider) {
            return Err(LlmError::Other(format!(
                "embed provider '{embed_provider}' not found in providers"
            )));
        }
        Ok(Self {
            routes,
            providers,
            default_provider,
            embed_provider,
            status_tx: None,
        })
    }

    pub fn set_status_tx(&mut self, tx: StatusTx) {
        for provider in self.providers.values_mut() {
            provider.set_status_tx(tx.clone());
        }
        self.status_tx = Some(tx);
    }

    fn emit_status(&self, msg: impl Into<String>) {
        if let Some(ref tx) = self.status_tx {
            let _ = tx.send(msg.into());
        }
    }

    #[must_use]
    pub fn providers(&self) -> &HashMap<String, SubProvider> {
        &self.providers
    }

    #[cfg(test)]
    fn select_provider(&self, messages: &[Message]) -> &SubProvider {
        let task = TaskType::classify(messages);
        tracing::debug!("classified task as {task:?}");

        if let Some(chain) = self.routes.get(&task) {
            for name in chain {
                if let Some(provider) = self.providers.get(name) {
                    return provider;
                }
            }
        }

        self.providers
            .get(&self.default_provider)
            .expect("default provider must exist")
    }

    async fn chat_with_fallback(&self, messages: &[Message]) -> Result<String, LlmError> {
        let task = TaskType::classify(messages);
        let chain = self
            .routes
            .get(&task)
            .or_else(|| self.routes.get(&TaskType::General))
            .ok_or(LlmError::NoRoute)?;

        let mut tried: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut last_error = None;
        for name in chain {
            let Some(provider) = self.providers.get(name) else {
                continue;
            };
            tried.insert(name);
            match provider.chat(messages).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    self.emit_status(format!("Provider {name} failed, trying next..."));
                    tracing::warn!("provider {name} failed: {e:#}, trying next");
                    last_error = Some(e);
                }
            }
        }

        if !tried.contains(self.default_provider.as_str())
            && let Some(provider) = self.providers.get(&self.default_provider)
        {
            self.emit_status(format!(
                "Falling back to default provider {}",
                self.default_provider
            ));
            tracing::info!("falling back to default provider {}", self.default_provider);
            match provider.chat(messages).await {
                Ok(response) => return Ok(response),
                Err(e) => last_error = Some(e),
            }
        }

        Err(last_error.unwrap_or(LlmError::NoProviders))
    }

    async fn stream_with_fallback(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        let task = TaskType::classify(messages);
        let chain = self
            .routes
            .get(&task)
            .or_else(|| self.routes.get(&TaskType::General))
            .ok_or(LlmError::NoRoute)?;

        let mut tried: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut last_error = None;
        for name in chain {
            let Some(provider) = self.providers.get(name) else {
                continue;
            };
            tried.insert(name);
            match provider.chat_stream(messages).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    self.emit_status(format!("Provider {name} failed, trying next..."));
                    tracing::warn!("provider {name} stream failed: {e:#}, trying next");
                    last_error = Some(e);
                }
            }
        }

        if !tried.contains(self.default_provider.as_str())
            && let Some(provider) = self.providers.get(&self.default_provider)
        {
            self.emit_status(format!(
                "Falling back to default provider {}",
                self.default_provider
            ));
            tracing::info!(
                "falling back to default provider {} for stream",
                self.default_provider
            );
            match provider.chat_stream(messages).await {
                Ok(stream) => return Ok(stream),
                Err(e) => last_error = Some(e),
            }
        }

        Err(last_error.unwrap_or(LlmError::NoProviders))
    }
}

impl LlmProvider for ModelOrchestrator {
    fn context_window(&self) -> Option<usize> {
        self.providers
            .get(&self.default_provider)
            .and_then(LlmProvider::context_window)
    }

    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        self.chat_with_fallback(messages).await
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        self.stream_with_fallback(messages).await
    }

    fn supports_streaming(&self) -> bool {
        self.providers
            .get(&self.default_provider)
            .is_some_and(LlmProvider::supports_streaming)
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        let provider = self
            .providers
            .get(&self.embed_provider)
            .ok_or(LlmError::NoProviders)?;
        provider.embed(text).await
    }

    fn supports_embeddings(&self) -> bool {
        self.providers
            .get(&self.embed_provider)
            .is_some_and(LlmProvider::supports_embeddings)
    }

    fn supports_tool_use(&self) -> bool {
        self.providers
            .get(&self.default_provider)
            .is_some_and(LlmProvider::supports_tool_use)
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        let provider = self
            .providers
            .get(&self.default_provider)
            .ok_or(LlmError::NoProviders)?;
        tracing::debug!(
            default_provider = %self.default_provider,
            tool_count = tools.len(),
            provider_supports_tool_use = provider.supports_tool_use(),
            "orchestrator delegating chat_with_tools"
        );
        provider.chat_with_tools(messages, tools).await
    }

    fn name(&self) -> &'static str {
        "orchestrator"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::ClaudeProvider;
    use crate::ollama::OllamaProvider;
    use crate::provider::Role;

    fn user_msg(content: &str) -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: content.into(),
            parts: vec![],
        }]
    }

    #[test]
    fn orchestrator_requires_valid_providers() {
        let providers = HashMap::new();
        let routes = HashMap::new();
        let result = ModelOrchestrator::new(routes, providers, "missing".into(), "missing".into());
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn orchestrator_routes_to_correct_provider() {
        let ollama = SubProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "test-embed".into(),
        ));
        let mut providers = HashMap::new();
        providers.insert("ollama".into(), ollama);

        let mut routes = HashMap::new();
        routes.insert(TaskType::General, vec!["ollama".into()]);
        routes.insert(TaskType::Coding, vec!["ollama".into()]);

        let orch =
            ModelOrchestrator::new(routes, providers, "ollama".into(), "ollama".into()).unwrap();

        assert_eq!(orch.name(), "orchestrator");
        assert!(orch.supports_streaming());
        assert!(orch.supports_embeddings());

        let provider = orch.select_provider(&user_msg("write code"));
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn orchestrator_missing_default_provider() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://localhost:11434",
                "test".into(),
                "test-embed".into(),
            )),
        );
        let result =
            ModelOrchestrator::new(HashMap::new(), providers, "missing".into(), "ollama".into());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("default provider 'missing' not found")
        );
    }

    #[test]
    fn orchestrator_missing_embed_provider() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://localhost:11434",
                "test".into(),
                "test-embed".into(),
            )),
        );
        let result =
            ModelOrchestrator::new(HashMap::new(), providers, "ollama".into(), "missing".into());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("embed provider 'missing' not found")
        );
    }

    #[test]
    fn orchestrator_providers_accessor() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://localhost:11434",
                "test".into(),
                "embed".into(),
            )),
        );
        let orch =
            ModelOrchestrator::new(HashMap::new(), providers, "ollama".into(), "ollama".into())
                .unwrap();
        assert_eq!(orch.providers().len(), 1);
        assert!(orch.providers().contains_key("ollama"));
    }

    #[test]
    fn orchestrator_select_falls_back_to_default() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://localhost:11434",
                "test".into(),
                "embed".into(),
            )),
        );
        let orch =
            ModelOrchestrator::new(HashMap::new(), providers, "ollama".into(), "ollama".into())
                .unwrap();
        let provider = orch.select_provider(&user_msg("hello world"));
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn orchestrator_select_skips_missing_in_chain() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://localhost:11434",
                "test".into(),
                "embed".into(),
            )),
        );
        let mut routes = HashMap::new();
        routes.insert(
            TaskType::General,
            vec!["nonexistent".into(), "ollama".into()],
        );
        let orch =
            ModelOrchestrator::new(routes, providers, "ollama".into(), "ollama".into()).unwrap();
        let provider = orch.select_provider(&user_msg("hello"));
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn orchestrator_clone() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://localhost:11434",
                "test".into(),
                "embed".into(),
            )),
        );
        let orch =
            ModelOrchestrator::new(HashMap::new(), providers, "ollama".into(), "ollama".into())
                .unwrap();
        let cloned = orch.clone();
        assert_eq!(cloned.name(), "orchestrator");
        assert_eq!(cloned.providers().len(), 1);
    }

    #[test]
    fn orchestrator_debug() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://localhost:11434",
                "test".into(),
                "embed".into(),
            )),
        );
        let orch =
            ModelOrchestrator::new(HashMap::new(), providers, "ollama".into(), "ollama".into())
                .unwrap();
        let debug = format!("{orch:?}");
        assert!(debug.contains("ModelOrchestrator"));
    }

    #[test]
    fn orchestrator_supports_streaming_delegates_to_default() {
        let mut providers = HashMap::new();
        providers.insert(
            "claude".into(),
            SubProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024)),
        );
        let orch =
            ModelOrchestrator::new(HashMap::new(), providers, "claude".into(), "claude".into())
                .unwrap();
        assert!(orch.supports_streaming());
    }

    #[test]
    fn orchestrator_supports_embeddings_delegates_to_embed_provider() {
        let mut providers = HashMap::new();
        providers.insert(
            "claude".into(),
            SubProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024)),
        );
        let orch =
            ModelOrchestrator::new(HashMap::new(), providers, "claude".into(), "claude".into())
                .unwrap();
        assert!(!orch.supports_embeddings());
    }

    #[tokio::test]
    async fn chat_with_fallback_single_provider_unreachable() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://127.0.0.1:1",
                "test".into(),
                "test".into(),
            )),
        );
        let mut routes = HashMap::new();
        routes.insert(TaskType::General, vec!["ollama".into()]);
        let orch =
            ModelOrchestrator::new(routes, providers, "ollama".into(), "ollama".into()).unwrap();

        let result = orch.chat(&user_msg("hello")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn chat_with_fallback_falls_through_chain() {
        let mut providers = HashMap::new();
        providers.insert(
            "bad".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://127.0.0.1:1",
                "test".into(),
                "test".into(),
            )),
        );
        providers.insert(
            "also-bad".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://127.0.0.1:2",
                "test".into(),
                "test".into(),
            )),
        );
        let mut routes = HashMap::new();
        routes.insert(TaskType::General, vec!["bad".into(), "also-bad".into()]);
        let orch = ModelOrchestrator::new(routes, providers, "bad".into(), "bad".into()).unwrap();

        let result = orch.chat(&user_msg("hello")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn chat_with_fallback_skips_missing_provider_in_chain() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://127.0.0.1:1",
                "test".into(),
                "test".into(),
            )),
        );
        let mut routes = HashMap::new();
        routes.insert(
            TaskType::General,
            vec!["nonexistent".into(), "ollama".into()],
        );
        let orch =
            ModelOrchestrator::new(routes, providers, "ollama".into(), "ollama".into()).unwrap();

        let result = orch.chat(&user_msg("hello")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn chat_with_fallback_no_route_configured() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://127.0.0.1:1",
                "test".into(),
                "test".into(),
            )),
        );
        let orch =
            ModelOrchestrator::new(HashMap::new(), providers, "ollama".into(), "ollama".into())
                .unwrap();

        let result = orch.chat(&user_msg("hello")).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no route configured")
        );
    }

    #[tokio::test]
    async fn stream_with_fallback_no_route_configured() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://127.0.0.1:1",
                "test".into(),
                "test".into(),
            )),
        );
        let orch =
            ModelOrchestrator::new(HashMap::new(), providers, "ollama".into(), "ollama".into())
                .unwrap();

        let result = orch.chat_stream(&user_msg("hello")).await;
        match result {
            Err(e) => assert!(e.to_string().contains("no route configured")),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn stream_with_fallback_all_fail() {
        let mut providers = HashMap::new();
        providers.insert(
            "bad".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://127.0.0.1:1",
                "test".into(),
                "test".into(),
            )),
        );
        let mut routes = HashMap::new();
        routes.insert(TaskType::General, vec!["bad".into()]);
        let orch = ModelOrchestrator::new(routes, providers, "bad".into(), "bad".into()).unwrap();

        let result = orch.chat_stream(&user_msg("hello")).await;
        assert!(matches!(result, Err(_)));
    }

    #[tokio::test]
    async fn embed_delegates_to_embed_provider() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://127.0.0.1:1",
                "test".into(),
                "test".into(),
            )),
        );
        let orch =
            ModelOrchestrator::new(HashMap::new(), providers, "ollama".into(), "ollama".into())
                .unwrap();

        let result = orch.embed("test text").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn chat_with_fallback_uses_general_route_as_fallback() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://127.0.0.1:1",
                "test".into(),
                "test".into(),
            )),
        );
        let mut routes = HashMap::new();
        routes.insert(TaskType::General, vec!["ollama".into()]);

        let orch =
            ModelOrchestrator::new(routes, providers, "ollama".into(), "ollama".into()).unwrap();

        let result = orch.chat(&user_msg("write a function to sort")).await;
        assert!(result.is_err());
    }

    #[test]
    fn orchestrator_select_uses_task_specific_route() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://localhost:11434",
                "test".into(),
                "embed".into(),
            )),
        );
        providers.insert(
            "claude".into(),
            SubProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024)),
        );
        let mut routes = HashMap::new();
        routes.insert(TaskType::Coding, vec!["claude".into()]);
        routes.insert(TaskType::General, vec!["ollama".into()]);

        let orch =
            ModelOrchestrator::new(routes, providers, "ollama".into(), "ollama".into()).unwrap();

        let provider = orch.select_provider(&user_msg("implement a function"));
        assert_eq!(provider.name(), "claude");

        let provider = orch.select_provider(&user_msg("hello there"));
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn orchestrator_context_window_delegates_to_default() {
        let mut providers = HashMap::new();
        providers.insert(
            "ollama".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://localhost:11434",
                "test".into(),
                "embed".into(),
            )),
        );

        let orch =
            ModelOrchestrator::new(HashMap::new(), providers, "ollama".into(), "ollama".into())
                .unwrap();

        let window = orch.context_window();
        assert_eq!(
            window,
            OllamaProvider::new("http://localhost:11434", "test".into(), "e".into())
                .context_window()
        );
    }
}
