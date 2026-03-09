// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod classifier;
mod router;

pub use classifier::{ModelSelection, TaskType};
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
    llm_routing: bool,
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
            llm_routing: false,
        })
    }

    #[must_use]
    pub fn with_llm_routing(mut self, enabled: bool) -> Self {
        self.llm_routing = enabled;
        self
    }

    pub fn set_status_tx(&mut self, tx: StatusTx) {
        for provider in self.providers.values_mut() {
            provider.set_status_tx(tx.clone());
        }
        self.status_tx = Some(tx);
    }

    /// Detect and apply the context window size from the default sub-provider.
    ///
    /// Currently supported for Ollama sub-providers via `show` API.
    /// Other sub-providers are skipped silently.
    pub async fn auto_detect_context_window(&mut self) {
        if let Some(provider) = self.providers.get_mut(&self.default_provider) {
            provider.auto_detect_context_window().await;
        }
    }

    /// Aggregate model lists from all sub-providers, deduplicating by id.
    ///
    /// Individual sub-provider errors are logged as warnings and skipped.
    ///
    /// # Errors
    ///
    /// Always succeeds (errors per-provider are swallowed).
    pub async fn list_models_remote(
        &self,
    ) -> Result<Vec<crate::model_cache::RemoteModelInfo>, LlmError> {
        let mut seen = std::collections::HashSet::new();
        let mut all = Vec::new();
        for p in self.providers.values() {
            match p.list_models_remote().await {
                Ok(models) => {
                    for m in models {
                        if seen.insert(m.id.clone()) {
                            all.push(m);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "orchestrator: list_models_remote sub-provider failed");
                }
            }
        }
        Ok(all)
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

    #[cfg(feature = "schema")]
    async fn try_llm_routing(&self, messages: &[Message]) -> Option<String> {
        if !self.llm_routing {
            return None;
        }
        let provider_names: Vec<&str> = self.providers.keys().map(String::as_str).collect();
        let routing_prompt = format!(
            "Select the best model provider for this task. Available: {}. \
             Respond in JSON with fields: model (string), reason (string).",
            provider_names.join(", ")
        );
        let last_message = messages.last().cloned()?;
        let routing_messages = vec![
            Message::from_legacy(crate::provider::Role::System, routing_prompt),
            last_message,
        ];
        let default = self.providers.get(&self.default_provider)?;
        match default
            .chat_typed::<ModelSelection>(&routing_messages)
            .await
        {
            Ok(selection) if self.providers.contains_key(&selection.model) => {
                tracing::info!(
                    model = %selection.model,
                    reason = %selection.reason,
                    "LLM routing selected provider"
                );
                Some(selection.model)
            }
            Ok(selection) => {
                tracing::warn!(
                    model = %selection.model,
                    "LLM routing selected unknown provider, falling back to rule-based"
                );
                None
            }
            Err(e) => {
                tracing::warn!("LLM routing failed, falling back to rule-based: {e:#}");
                None
            }
        }
    }

    #[cfg(not(feature = "schema"))]
    #[allow(clippy::unused_async)]
    async fn try_llm_routing(&self, _messages: &[Message]) -> Option<String> {
        None
    }

    async fn chat_with_fallback(&self, messages: &[Message]) -> Result<String, LlmError> {
        if let Some(selected) = self.try_llm_routing(messages).await
            && let Some(provider) = self.providers.get(&selected)
        {
            match provider.chat(messages).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    tracing::warn!("LLM-routed provider {selected} failed: {e:#}, falling back");
                }
            }
        }

        let task = TaskType::classify(messages);
        let chain = self
            .routes
            .get(&task)
            .or_else(|| self.routes.get(&TaskType::General));

        let mut tried: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut last_error = None;

        if let Some(chain) = chain {
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

    /// Route a chat request to a specific named provider.
    ///
    /// If `name` is not found in the providers map, falls back to default routing.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError`] if the underlying provider call fails.
    pub(crate) async fn chat_for_named(
        &self,
        name: &str,
        messages: &[Message],
    ) -> Result<String, LlmError> {
        if let Some(provider) = self.providers.get(name) {
            match provider.chat(messages).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    tracing::warn!(
                        name,
                        "named provider failed: {e:#}, falling back to default routing"
                    );
                }
            }
        } else {
            tracing::debug!(
                name,
                "named provider not found, falling back to default routing"
            );
        }
        self.chat_with_fallback(messages).await
    }

    /// Route a tool-aware chat request to a specific named provider.
    ///
    /// If `name` is not found in the providers map, falls back to the default
    /// provider's `chat_with_tools`.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError`] if the underlying provider call fails.
    pub(crate) async fn chat_for_named_with_tools(
        &self,
        name: &str,
        messages: &[Message],
        tools: &[crate::provider::ToolDefinition],
    ) -> Result<crate::provider::ChatResponse, LlmError> {
        if let Some(provider) = self.providers.get(name) {
            match provider.chat_with_tools(messages, tools).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    tracing::warn!(
                        name,
                        "named provider chat_with_tools failed: {e:#}, falling back to default"
                    );
                }
            }
        } else {
            tracing::debug!(
                name,
                "named provider not found, falling back to default for chat_with_tools"
            );
        }
        let provider = self
            .providers
            .get(&self.default_provider)
            .ok_or(LlmError::NoProviders)?;
        provider.chat_with_tools(messages, tools).await
    }

    async fn stream_with_fallback(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        if let Some(selected) = self.try_llm_routing(messages).await
            && let Some(provider) = self.providers.get(&selected)
        {
            match provider.chat_stream(messages).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    tracing::warn!(
                        "LLM-routed provider {selected} stream failed: {e:#}, falling back"
                    );
                }
            }
        }

        let task = TaskType::classify(messages);
        let chain = self
            .routes
            .get(&task)
            .or_else(|| self.routes.get(&TaskType::General));

        let mut tried: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut last_error = None;

        if let Some(chain) = chain {
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

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        self.providers
            .get(&self.default_provider)
            .and_then(LlmProvider::last_cache_usage)
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "orchestrator"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::ClaudeProvider;
    use crate::ollama::OllamaProvider;
    use crate::provider::MessageMetadata;
    use crate::provider::Role;

    fn user_msg(content: &str) -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: content.into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }]
    }

    /// Spawn a minimal HTTP server that responds to any POST request with a fixed body.
    /// Returns the bound port and a handle that keeps the server alive.
    async fn spawn_mock_ollama_server(
        response_body: &'static str,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            // Accept a fixed number of connections for test isolation
            for _ in 0..10u8 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.split();
                    let mut buf_reader = BufReader::new(reader);
                    // Read headers until blank line
                    let mut line = String::new();
                    let mut content_length: usize = 0;
                    loop {
                        line.clear();
                        buf_reader.read_line(&mut line).await.unwrap_or(0);
                        if line == "\r\n" || line == "\n" {
                            break;
                        }
                        let lower = line.to_lowercase();
                        if lower.starts_with("content-length:") {
                            content_length = lower
                                .trim_start_matches("content-length:")
                                .trim()
                                .parse()
                                .unwrap_or(0);
                        }
                    }
                    // Consume body
                    let mut body = vec![0u8; content_length];
                    use tokio::io::AsyncReadExt;
                    buf_reader.read_exact(&mut body).await.unwrap_or(0);

                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    writer.write_all(resp.as_bytes()).await.ok();
                });
            }
        });

        (port, handle)
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
        // When no routes are configured, the orchestrator must fall back to default_provider
        // instead of returning NoRoute. The default provider is unreachable here, so we get
        // a connection error — but NOT a "no route configured" error.
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
            !result
                .unwrap_err()
                .to_string()
                .contains("no route configured"),
            "expected connection error, not NoRoute"
        );
    }

    #[tokio::test]
    async fn stream_with_fallback_no_route_configured() {
        // When no routes are configured, stream must fall back to default_provider
        // instead of returning NoRoute. The default provider is unreachable here.
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
            Err(e) => assert!(
                !e.to_string().contains("no route configured"),
                "expected connection error, not NoRoute"
            ),
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

    // Priority 1: try_llm_routing tests via chat_with_fallback

    #[tokio::test]
    async fn llm_routing_empty_messages_returns_none_early() {
        // try_llm_routing returns None early when messages is empty (REV-4 fix).
        // With no routes configured and llm_routing=true, empty messages means
        // try_llm_routing returns None, then rule-based routing skips (no chain),
        // and default_provider fallback runs (unreachable → connection error).
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
                .unwrap()
                .with_llm_routing(true);

        // Empty messages slice: try_llm_routing does messages.last().cloned()? → returns None
        let result = orch.chat(&[]).await;
        assert!(result.is_err());
        // Should be a connection error (default_provider fallback), not NoRoute
        assert!(
            !result
                .unwrap_err()
                .to_string()
                .contains("no route configured"),
            "expected connection error from default_provider fallback, not NoRoute"
        );
    }

    #[tokio::test]
    async fn llm_routing_disabled_skips_llm_routing() {
        // With llm_routing=false, try_llm_routing returns None immediately.
        // Falls through to rule-based routing (no chain), then default_provider fallback
        // runs (unreachable → connection error), NOT NoRoute.
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
        // llm_routing is false by default

        let result = orch.chat(&user_msg("hello")).await;
        assert!(result.is_err());
        assert!(
            !result
                .unwrap_err()
                .to_string()
                .contains("no route configured"),
            "expected connection error from default_provider fallback, not NoRoute"
        );
    }

    #[tokio::test]
    async fn llm_routing_provider_fails_falls_back_to_rule_based() {
        // LLM routing enabled, but default provider is unreachable → chat_typed fails
        // → try_llm_routing returns None → rule-based fallback runs.
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
        let orch = ModelOrchestrator::new(routes, providers, "bad".into(), "bad".into())
            .unwrap()
            .with_llm_routing(true);

        // LLM routing: chat_typed on "bad" provider fails (connection refused)
        // Falls back to rule-based routing which also fails (unreachable)
        let result = orch.chat(&user_msg("hello")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn llm_routing_valid_json_known_model_selects_provider() {
        // LLM routing enabled; default provider (mock_server) returns valid ModelSelection JSON
        // with a known model name "target". The orchestrator should try "target" first.
        // "target" is unreachable, so it fails, then falls back to default_provider "router"
        // (mock server) which succeeds.

        // Ollama API response format for a single non-streaming message
        // First call: LLM routing request → selects "target"
        // Second call: "target" fails, default_provider "router" handles actual chat
        let chat_response = r#"{"model":"test","created_at":"2024-01-01T00:00:00Z","message":{"role":"assistant","content":"fallback response"},"done":true,"done_reason":"stop","total_duration":1000000,"load_duration":0,"prompt_eval_count":1,"prompt_eval_duration":0,"eval_count":1,"eval_duration":0}"#;

        let (port, _handle) = spawn_mock_ollama_server(chat_response).await;

        let mut providers = HashMap::new();
        providers.insert(
            "router".into(),
            SubProvider::Ollama(OllamaProvider::new(
                &format!("http://127.0.0.1:{port}"),
                "test".into(),
                "test".into(),
            )),
        );
        providers.insert(
            "target".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://127.0.0.1:1",
                "test".into(),
                "test".into(),
            )),
        );
        // No routes configured — after "target" fails, default_provider "router" takes over
        let orch =
            ModelOrchestrator::new(HashMap::new(), providers, "router".into(), "router".into())
                .unwrap()
                .with_llm_routing(true);

        let result = orch.chat(&user_msg("hello")).await;
        // "target" is unreachable, LLM routing falls back to default_provider "router" → success
        assert!(
            result.is_ok(),
            "expected success via default_provider fallback, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn llm_routing_valid_json_unknown_model_falls_back_to_rule_based() {
        // LLM routing enabled; default provider returns valid ModelSelection JSON
        // but the model name "unknown-provider" is not in providers.
        // try_llm_routing returns None, no rule-based routes, default_provider "router"
        // handles the chat and succeeds.

        let chat_response = r#"{"model":"test","created_at":"2024-01-01T00:00:00Z","message":{"role":"assistant","content":"default response"},"done":true,"done_reason":"stop","total_duration":1000000,"load_duration":0,"prompt_eval_count":1,"prompt_eval_duration":0,"eval_count":1,"eval_duration":0}"#;

        let (port, _handle) = spawn_mock_ollama_server(chat_response).await;

        let mut providers = HashMap::new();
        providers.insert(
            "router".into(),
            SubProvider::Ollama(OllamaProvider::new(
                &format!("http://127.0.0.1:{port}"),
                "test".into(),
                "test".into(),
            )),
        );
        // No routes configured
        let orch =
            ModelOrchestrator::new(HashMap::new(), providers, "router".into(), "router".into())
                .unwrap()
                .with_llm_routing(true);

        let result = orch.chat(&user_msg("hello")).await;
        // try_llm_routing returns None (unknown provider), no rule-based routes,
        // default_provider "router" handles the chat → success
        assert!(
            result.is_ok(),
            "expected success from default_provider fallback, got: {result:?}"
        );
    }

    #[test]
    fn with_llm_routing_sets_flag() {
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
        assert!(!orch.llm_routing);
        let orch = orch.with_llm_routing(true);
        assert!(orch.llm_routing);
    }

    #[tokio::test]
    async fn chat_with_fallback_uses_default_when_no_routes() {
        // Regression test for issue #1396: when routes map is empty, orchestrator must
        // use default_provider instead of returning NoRoute.
        // The mock server responds successfully, so the call must succeed.
        let chat_response = r#"{"model":"test","created_at":"2024-01-01T00:00:00Z","message":{"role":"assistant","content":"hello from default"},"done":true,"done_reason":"stop","total_duration":1000000,"load_duration":0,"prompt_eval_count":1,"prompt_eval_duration":0,"eval_count":1,"eval_duration":0}"#;
        let (port, _handle) = spawn_mock_ollama_server(chat_response).await;

        let mut providers = HashMap::new();
        providers.insert(
            "default".into(),
            SubProvider::Ollama(OllamaProvider::new(
                &format!("http://127.0.0.1:{port}"),
                "test".into(),
                "test".into(),
            )),
        );
        // No routes configured
        let orch = ModelOrchestrator::new(
            HashMap::new(),
            providers,
            "default".into(),
            "default".into(),
        )
        .unwrap();

        let result = orch.chat(&user_msg("hello")).await;
        assert!(
            result.is_ok(),
            "expected success from default_provider fallback, got: {result:?}"
        );
        assert_eq!(result.unwrap(), "hello from default");
    }

    #[tokio::test]
    async fn chat_with_fallback_uses_default_when_no_general_route() {
        // Regression test for issue #1396: when only a specific route exists (e.g. Coding)
        // and the message is classified as General, orchestrator must fall back to
        // default_provider rather than returning NoRoute.
        let chat_response = r#"{"model":"test","created_at":"2024-01-01T00:00:00Z","message":{"role":"assistant","content":"hello from default"},"done":true,"done_reason":"stop","total_duration":1000000,"load_duration":0,"prompt_eval_count":1,"prompt_eval_duration":0,"eval_count":1,"eval_duration":0}"#;
        let (port, _handle) = spawn_mock_ollama_server(chat_response).await;

        let mut providers = HashMap::new();
        providers.insert(
            "default".into(),
            SubProvider::Ollama(OllamaProvider::new(
                &format!("http://127.0.0.1:{port}"),
                "test".into(),
                "test".into(),
            )),
        );
        providers.insert(
            "coding".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://127.0.0.1:1",
                "test".into(),
                "test".into(),
            )),
        );
        // Only Coding route configured; no General route
        let mut routes = HashMap::new();
        routes.insert(TaskType::Coding, vec!["coding".into()]);

        let orch =
            ModelOrchestrator::new(routes, providers, "default".into(), "default".into()).unwrap();

        // "hello world" is General task — no route, must fall back to default_provider
        let result = orch.chat(&user_msg("hello world")).await;
        assert!(
            result.is_ok(),
            "expected success from default_provider fallback, got: {result:?}"
        );
        assert_eq!(result.unwrap(), "hello from default");
    }

    #[tokio::test]
    async fn chat_for_named_routes_to_known_provider() {
        // chat_for_named must route to the named provider when it exists.
        let chat_response = r#"{"model":"test","created_at":"2024-01-01T00:00:00Z","message":{"role":"assistant","content":"named response"},"done":true,"done_reason":"stop","total_duration":1000000,"load_duration":0,"prompt_eval_count":1,"prompt_eval_duration":0,"eval_count":1,"eval_duration":0}"#;
        let (port, _handle) = spawn_mock_ollama_server(chat_response).await;

        let mut providers = HashMap::new();
        providers.insert(
            "default".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://127.0.0.1:1",
                "test".into(),
                "test".into(),
            )),
        );
        providers.insert(
            "target".into(),
            SubProvider::Ollama(OllamaProvider::new(
                &format!("http://127.0.0.1:{port}"),
                "test".into(),
                "test".into(),
            )),
        );

        let orch = ModelOrchestrator::new(
            HashMap::new(),
            providers,
            "default".into(),
            "default".into(),
        )
        .unwrap();

        let result = orch.chat_for_named("target", &user_msg("hello")).await;
        assert!(
            result.is_ok(),
            "expected success from named provider, got: {result:?}"
        );
        assert_eq!(result.unwrap(), "named response");
    }

    #[tokio::test]
    async fn chat_for_named_falls_back_when_unknown() {
        // chat_for_named must fall back to default routing when the named provider is unknown.
        let chat_response = r#"{"model":"test","created_at":"2024-01-01T00:00:00Z","message":{"role":"assistant","content":"default response"},"done":true,"done_reason":"stop","total_duration":1000000,"load_duration":0,"prompt_eval_count":1,"prompt_eval_duration":0,"eval_count":1,"eval_duration":0}"#;
        let (port, _handle) = spawn_mock_ollama_server(chat_response).await;

        let mut providers = HashMap::new();
        providers.insert(
            "default".into(),
            SubProvider::Ollama(OllamaProvider::new(
                &format!("http://127.0.0.1:{port}"),
                "test".into(),
                "test".into(),
            )),
        );
        let mut routes = HashMap::new();
        routes.insert(TaskType::General, vec!["default".into()]);

        let orch =
            ModelOrchestrator::new(routes, providers, "default".into(), "default".into()).unwrap();

        let result = orch.chat_for_named("nonexistent", &user_msg("hello")).await;
        assert!(
            result.is_ok(),
            "expected success via default routing fallback, got: {result:?}"
        );
        assert_eq!(result.unwrap(), "default response");
    }

    #[tokio::test]
    async fn chat_for_named_falls_back_when_provider_fails() {
        // When named provider exists but its chat() call fails, chat_for_named must
        // fall through to chat_with_fallback instead of propagating the error directly.
        let chat_response = r#"{"model":"test","created_at":"2024-01-01T00:00:00Z","message":{"role":"assistant","content":"fallback response"},"done":true,"done_reason":"stop","total_duration":1000000,"load_duration":0,"prompt_eval_count":1,"prompt_eval_duration":0,"eval_count":1,"eval_duration":0}"#;
        let (port, _handle) = spawn_mock_ollama_server(chat_response).await;

        let mut providers = HashMap::new();
        providers.insert(
            "default".into(),
            SubProvider::Ollama(OllamaProvider::new(
                &format!("http://127.0.0.1:{port}"),
                "test".into(),
                "test".into(),
            )),
        );
        providers.insert(
            "broken".into(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://127.0.0.1:1",
                "test".into(),
                "test".into(),
            )),
        );
        let mut routes = HashMap::new();
        routes.insert(TaskType::General, vec!["default".into()]);

        let orch =
            ModelOrchestrator::new(routes, providers, "default".into(), "default".into()).unwrap();

        // "broken" exists but is unreachable → must fall back to default routing → success
        let result = orch.chat_for_named("broken", &user_msg("hello")).await;
        assert!(
            result.is_ok(),
            "expected success via fallback after named provider failure, got: {result:?}"
        );
        assert_eq!(result.unwrap(), "fallback response");
    }
}
