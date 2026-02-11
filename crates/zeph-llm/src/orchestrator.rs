use std::collections::HashMap;

use anyhow::{Context, Result};

#[cfg(feature = "candle")]
use crate::candle_provider::CandleProvider;
use crate::claude::ClaudeProvider;
use crate::ollama::OllamaProvider;
use crate::provider::{ChatStream, LlmProvider, Message, Role};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskType {
    Coding,
    Creative,
    Analysis,
    Translation,
    Summarization,
    General,
}

impl TaskType {
    #[must_use]
    pub fn classify(messages: &[Message]) -> Self {
        let last_user_msg = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.to_lowercase())
            .unwrap_or_default();

        if contains_code_indicators(&last_user_msg) {
            Self::Coding
        } else if contains_translation_indicators(&last_user_msg) {
            Self::Translation
        } else if contains_summary_indicators(&last_user_msg) {
            Self::Summarization
        } else if contains_creative_indicators(&last_user_msg) {
            Self::Creative
        } else if contains_analysis_indicators(&last_user_msg) {
            Self::Analysis
        } else {
            Self::General
        }
    }

    #[must_use]
    pub fn parse_str(s: &str) -> Self {
        match s {
            "coding" => Self::Coding,
            "creative" => Self::Creative,
            "analysis" => Self::Analysis,
            "translation" => Self::Translation,
            "summarization" => Self::Summarization,
            _ => Self::General,
        }
    }
}

fn contains_code_indicators(text: &str) -> bool {
    const INDICATORS: &[&str] = &[
        "code",
        "function",
        "implement",
        "debug",
        "compile",
        "syntax",
        "refactor",
        "algorithm",
        "class",
        "struct",
        "enum",
        "trait",
        "bug",
        "error",
        "fix",
        "rust",
        "python",
        "javascript",
        "typescript",
        "```",
        "fn ",
        "def ",
        "async fn",
        "pub fn",
    ];
    INDICATORS.iter().any(|kw| text.contains(kw))
}

fn contains_translation_indicators(text: &str) -> bool {
    const INDICATORS: &[&str] = &[
        "translate",
        "translation",
        "переведи",
        "перевод",
        "to english",
        "to russian",
        "to spanish",
        "to french",
        "на английский",
        "на русский",
    ];
    INDICATORS.iter().any(|kw| text.contains(kw))
}

fn contains_summary_indicators(text: &str) -> bool {
    const INDICATORS: &[&str] = &[
        "summarize",
        "summary",
        "tldr",
        "tl;dr",
        "brief",
        "кратко",
        "резюме",
        "суммируй",
    ];
    INDICATORS.iter().any(|kw| text.contains(kw))
}

fn contains_creative_indicators(text: &str) -> bool {
    const INDICATORS: &[&str] = &[
        "write a story",
        "poem",
        "creative",
        "imagine",
        "fiction",
        "narrative",
        "compose",
        "стих",
        "рассказ",
        "сочини",
    ];
    INDICATORS.iter().any(|kw| text.contains(kw))
}

fn contains_analysis_indicators(text: &str) -> bool {
    const INDICATORS: &[&str] = &[
        "analyze",
        "analysis",
        "compare",
        "evaluate",
        "assess",
        "review",
        "critique",
        "examine",
        "pros and cons",
        "анализ",
        "сравни",
        "оцени",
    ];
    INDICATORS.iter().any(|kw| text.contains(kw))
}

/// Inner provider enum without the Orchestrator variant to break recursive type cycles.
#[derive(Debug, Clone)]
pub enum SubProvider {
    Ollama(OllamaProvider),
    Claude(ClaudeProvider),
    #[cfg(feature = "candle")]
    Candle(CandleProvider),
}

impl LlmProvider for SubProvider {
    async fn chat(&self, messages: &[Message]) -> Result<String> {
        match self {
            Self::Ollama(p) => p.chat(messages).await,
            Self::Claude(p) => p.chat(messages).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.chat(messages).await,
        }
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream> {
        match self {
            Self::Ollama(p) => p.chat_stream(messages).await,
            Self::Claude(p) => p.chat_stream(messages).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.chat_stream(messages).await,
        }
    }

    fn supports_streaming(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_streaming(),
            Self::Claude(p) => p.supports_streaming(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.supports_streaming(),
        }
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        match self {
            Self::Ollama(p) => p.embed(text).await,
            Self::Claude(p) => p.embed(text).await,
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.embed(text).await,
        }
    }

    fn supports_embeddings(&self) -> bool {
        match self {
            Self::Ollama(p) => p.supports_embeddings(),
            Self::Claude(p) => p.supports_embeddings(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.supports_embeddings(),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Ollama(p) => p.name(),
            Self::Claude(p) => p.name(),
            #[cfg(feature = "candle")]
            Self::Candle(p) => p.name(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelOrchestrator {
    routes: HashMap<TaskType, Vec<String>>,
    providers: HashMap<String, SubProvider>,
    default_provider: String,
    embed_provider: String,
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
    ) -> Result<Self> {
        anyhow::ensure!(
            providers.contains_key(&default_provider),
            "default provider '{default_provider}' not found in providers"
        );
        anyhow::ensure!(
            providers.contains_key(&embed_provider),
            "embed provider '{embed_provider}' not found in providers"
        );
        Ok(Self {
            routes,
            providers,
            default_provider,
            embed_provider,
        })
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

    async fn chat_with_fallback(&self, messages: &[Message]) -> Result<String> {
        let task = TaskType::classify(messages);
        let chain = self
            .routes
            .get(&task)
            .or_else(|| self.routes.get(&TaskType::General))
            .context("no route configured")?;

        let mut last_error = None;
        for name in chain {
            let Some(provider) = self.providers.get(name) else {
                continue;
            };
            match provider.chat(messages).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    tracing::warn!("provider {name} failed: {e:#}, trying next");
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no providers available")))
    }

    async fn stream_with_fallback(&self, messages: &[Message]) -> Result<ChatStream> {
        let task = TaskType::classify(messages);
        let chain = self
            .routes
            .get(&task)
            .or_else(|| self.routes.get(&TaskType::General))
            .context("no route configured")?;

        let mut last_error = None;
        for name in chain {
            let Some(provider) = self.providers.get(name) else {
                continue;
            };
            match provider.chat_stream(messages).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    tracing::warn!("provider {name} stream failed: {e:#}, trying next");
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no providers available")))
    }
}

impl LlmProvider for ModelOrchestrator {
    async fn chat(&self, messages: &[Message]) -> Result<String> {
        self.chat_with_fallback(messages).await
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream> {
        self.stream_with_fallback(messages).await
    }

    fn supports_streaming(&self) -> bool {
        self.providers
            .get(&self.default_provider)
            .is_some_and(LlmProvider::supports_streaming)
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let provider = self
            .providers
            .get(&self.embed_provider)
            .context("embed provider not found")?;
        provider.embed(text).await
    }

    fn supports_embeddings(&self) -> bool {
        self.providers
            .get(&self.embed_provider)
            .is_some_and(LlmProvider::supports_embeddings)
    }

    fn name(&self) -> &'static str {
        "orchestrator"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    fn user_msg(content: &str) -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: content.into(),
        }]
    }

    #[test]
    fn classify_coding() {
        assert_eq!(
            TaskType::classify(&user_msg("write a function to sort")),
            TaskType::Coding
        );
        assert_eq!(
            TaskType::classify(&user_msg("debug this error")),
            TaskType::Coding
        );
        assert_eq!(
            TaskType::classify(&user_msg("implement a struct")),
            TaskType::Coding
        );
    }

    #[test]
    fn classify_translation() {
        assert_eq!(
            TaskType::classify(&user_msg("translate this to english")),
            TaskType::Translation
        );
    }

    #[test]
    fn classify_summarization() {
        assert_eq!(
            TaskType::classify(&user_msg("summarize this article")),
            TaskType::Summarization
        );
        assert_eq!(
            TaskType::classify(&user_msg("give me a tldr")),
            TaskType::Summarization
        );
    }

    #[test]
    fn classify_creative() {
        assert_eq!(
            TaskType::classify(&user_msg("write a story about a dragon")),
            TaskType::Creative
        );
        assert_eq!(
            TaskType::classify(&user_msg("compose a poem")),
            TaskType::Creative
        );
    }

    #[test]
    fn classify_analysis() {
        assert_eq!(
            TaskType::classify(&user_msg("analyze this data")),
            TaskType::Analysis
        );
        assert_eq!(
            TaskType::classify(&user_msg("compare these two approaches")),
            TaskType::Analysis
        );
    }

    #[test]
    fn classify_general() {
        assert_eq!(TaskType::classify(&user_msg("hello")), TaskType::General);
        assert_eq!(
            TaskType::classify(&user_msg("what time is it")),
            TaskType::General
        );
    }

    #[test]
    fn classify_empty_messages() {
        assert_eq!(TaskType::classify(&[]), TaskType::General);
    }

    #[test]
    fn task_type_from_str() {
        assert_eq!(TaskType::parse_str("coding"), TaskType::Coding);
        assert_eq!(TaskType::parse_str("creative"), TaskType::Creative);
        assert_eq!(TaskType::parse_str("analysis"), TaskType::Analysis);
        assert_eq!(TaskType::parse_str("translation"), TaskType::Translation);
        assert_eq!(
            TaskType::parse_str("summarization"),
            TaskType::Summarization
        );
        assert_eq!(TaskType::parse_str("general"), TaskType::General);
        assert_eq!(TaskType::parse_str("unknown"), TaskType::General);
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

    #[test]
    fn task_type_debug() {
        let task = TaskType::Coding;
        assert_eq!(format!("{task:?}"), "Coding");
    }

    #[test]
    fn task_type_copy_and_eq() {
        let a = TaskType::Creative;
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn task_type_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(TaskType::Coding);
        set.insert(TaskType::Coding);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn classify_uses_last_user_message() {
        let messages = vec![
            Message {
                role: Role::User,
                content: "write a function".into(),
            },
            Message {
                role: Role::Assistant,
                content: "here it is".into(),
            },
            Message {
                role: Role::User,
                content: "translate to spanish".into(),
            },
        ];
        assert_eq!(TaskType::classify(&messages), TaskType::Translation);
    }

    #[test]
    fn classify_ignores_system_messages() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "you write code".into(),
            },
            Message {
                role: Role::User,
                content: "hello there".into(),
            },
        ];
        assert_eq!(TaskType::classify(&messages), TaskType::General);
    }

    #[test]
    fn classify_code_indicators_comprehensive() {
        for keyword in &[
            "algorithm",
            "refactor",
            "compile",
            "syntax",
            "pub fn",
            "```",
        ] {
            let msgs = user_msg(keyword);
            assert_eq!(
                TaskType::classify(&msgs),
                TaskType::Coding,
                "failed for: {keyword}"
            );
        }
    }

    #[test]
    fn classify_summary_indicators() {
        assert_eq!(
            TaskType::classify(&user_msg("give me a tl;dr")),
            TaskType::Summarization
        );
        assert_eq!(
            TaskType::classify(&user_msg("brief overview please")),
            TaskType::Summarization
        );
    }

    #[test]
    fn classify_creative_indicators() {
        assert_eq!(
            TaskType::classify(&user_msg("imagine a world where")),
            TaskType::Creative
        );
    }

    #[test]
    fn classify_analysis_indicators() {
        assert_eq!(
            TaskType::classify(&user_msg("evaluate this approach")),
            TaskType::Analysis
        );
        assert_eq!(
            TaskType::classify(&user_msg("pros and cons of X")),
            TaskType::Analysis
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
    fn classify_russian_translation_indicator() {
        assert_eq!(
            TaskType::classify(&user_msg("переведи на английский")),
            TaskType::Translation
        );
    }

    #[test]
    fn classify_russian_summary_indicator() {
        assert_eq!(
            TaskType::classify(&user_msg("кратко опиши")),
            TaskType::Summarization
        );
    }

    #[test]
    fn classify_russian_creative_indicator() {
        assert_eq!(
            TaskType::classify(&user_msg("сочини рассказ")),
            TaskType::Creative
        );
    }

    #[test]
    fn classify_russian_analysis_indicator() {
        assert_eq!(
            TaskType::classify(&user_msg("анализ данных")),
            TaskType::Analysis
        );
    }

    #[test]
    fn classify_code_with_backticks() {
        assert_eq!(
            TaskType::classify(&user_msg("here is some code ```rust let x = 5;```")),
            TaskType::Coding
        );
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
}
