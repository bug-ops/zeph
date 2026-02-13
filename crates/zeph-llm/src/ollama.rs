use anyhow::Context;
use ollama_rs::Ollama;
use ollama_rs::generation::chat::ChatMessage;
use ollama_rs::generation::chat::request::ChatMessageRequest;
use ollama_rs::generation::embeddings::request::{EmbeddingsInput, GenerateEmbeddingsRequest};
use tokio_stream::StreamExt;

use crate::provider::{ChatStream, LlmProvider, Message, Role};

#[derive(Debug)]
pub struct ModelInfo {
    pub context_length: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct OllamaProvider {
    client: Ollama,
    model: String,
    embedding_model: String,
    context_window_size: Option<usize>,
}

impl OllamaProvider {
    #[must_use]
    pub fn new(base_url: &str, model: String, embedding_model: String) -> Self {
        let (host, port) = parse_host_port(base_url);
        Self {
            client: Ollama::new(host, port),
            model,
            embedding_model,
            context_window_size: None,
        }
    }

    /// Set context window size (typically from /api/show response).
    pub fn set_context_window(&mut self, size: usize) {
        self.context_window_size = Some(size);
    }

    /// Query Ollama /api/show for model metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails.
    pub async fn fetch_model_info(&self) -> anyhow::Result<ModelInfo> {
        let info = self
            .client
            .show_model_info(self.model.clone())
            .await
            .context("failed to fetch model info from Ollama")?;

        // Try model_info map first (newer ollama versions)
        let ctx = info
            .model_info
            .iter()
            .find_map(|(k, v)| {
                if k.ends_with(".context_length") {
                    v.as_u64().and_then(|n| usize::try_from(n).ok())
                } else {
                    None
                }
            })
            .or_else(|| parse_num_ctx(&info.parameters));

        Ok(ModelInfo {
            context_length: ctx,
        })
    }

    /// Check if Ollama is reachable.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection to Ollama fails.
    pub async fn health_check(&self) -> anyhow::Result<()> {
        self.client
            .list_local_models()
            .await
            .context("failed to connect to Ollama — is it running?")?;
        Ok(())
    }

    /// Send a minimal chat request to force Ollama to load the model into memory.
    ///
    /// # Errors
    ///
    /// Returns an error if the warmup request fails.
    pub async fn warmup(&self) -> anyhow::Result<()> {
        let request =
            ChatMessageRequest::new(self.model.clone(), vec![ChatMessage::user("hi".to_owned())]);
        self.client
            .send_chat_messages(request)
            .await
            .context("Ollama warmup failed")?;
        Ok(())
    }
}

impl LlmProvider for OllamaProvider {
    fn context_window(&self) -> Option<usize> {
        self.context_window_size
    }

    async fn chat(&self, messages: &[Message]) -> anyhow::Result<String> {
        let ollama_messages: Vec<ChatMessage> = messages.iter().map(convert_message).collect();

        let request = ChatMessageRequest::new(self.model.clone(), ollama_messages);

        let response = self
            .client
            .send_chat_messages(request)
            .await
            .context("Ollama chat request failed")?;

        Ok(response.message.content)
    }

    async fn chat_stream(&self, messages: &[Message]) -> anyhow::Result<ChatStream> {
        let ollama_messages: Vec<ChatMessage> = messages.iter().map(convert_message).collect();
        let request = ChatMessageRequest::new(self.model.clone(), ollama_messages);

        let stream = self
            .client
            .send_chat_messages_stream(request)
            .await
            .context("Ollama streaming request failed")?;

        let mapped = stream.map(|item| match item {
            Ok(response) => Ok(response.message.content),
            Err(()) => Err(anyhow::anyhow!("Ollama stream chunk failed")),
        });

        Ok(Box::pin(mapped))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let request = GenerateEmbeddingsRequest::new(
            self.embedding_model.clone(),
            EmbeddingsInput::from(text),
        );

        let response = self
            .client
            .generate_embeddings(request)
            .await
            .context("Ollama embedding request failed")?;

        response
            .embeddings
            .into_iter()
            .next()
            .context("empty embeddings response from Ollama")
    }

    fn supports_embeddings(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "ollama"
    }
}

fn convert_message(msg: &Message) -> ChatMessage {
    match msg.role {
        Role::System => ChatMessage::system(msg.content.clone()),
        Role::User => ChatMessage::user(msg.content.clone()),
        Role::Assistant => ChatMessage::assistant(msg.content.clone()),
    }
}

fn parse_num_ctx(parameters: &str) -> Option<usize> {
    for line in parameters.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("num_ctx")
            && let Ok(val) = rest.trim().parse::<usize>()
        {
            return Some(val);
        }
    }
    None
}

fn parse_host_port(url: &str) -> (String, u16) {
    let url = url.trim_end_matches('/');
    if let Some(colon_pos) = url.rfind(':') {
        let port_str = &url[colon_pos + 1..];
        if let Ok(port) = port_str.parse::<u16>() {
            let host = url[..colon_pos].to_string();
            return (host, port);
        }
    }
    (url.to_string(), 11434)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_window_none_by_default() {
        let provider = OllamaProvider::new("http://localhost:11434", "test".into(), "embed".into());
        assert!(provider.context_window().is_none());
    }

    #[test]
    fn context_window_after_set() {
        let mut provider =
            OllamaProvider::new("http://localhost:11434", "test".into(), "embed".into());
        provider.set_context_window(32768);
        assert_eq!(provider.context_window(), Some(32768));
    }

    #[test]
    fn parse_num_ctx_from_parameters() {
        assert_eq!(parse_num_ctx("num_ctx 4096"), Some(4096));
        assert_eq!(
            parse_num_ctx("num_ctx                    32768"),
            Some(32768)
        );
        assert_eq!(parse_num_ctx("other_param 123\nnum_ctx 8192"), Some(8192));
        assert!(parse_num_ctx("no match here").is_none());
        assert!(parse_num_ctx("").is_none());
    }

    #[test]
    fn parse_host_port_with_port() {
        let (host, port) = parse_host_port("http://localhost:11434");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_without_port() {
        let (host, port) = parse_host_port("http://localhost");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 11434);
    }

    #[test]
    fn convert_message_roles() {
        let msg = Message {
            role: Role::User,
            content: "hello".into(),
        };
        let cm = convert_message(&msg);
        assert_eq!(cm.content, "hello");
    }

    #[test]
    fn supports_streaming_returns_true() {
        let provider =
            OllamaProvider::new("http://localhost:11434", "test".into(), "test-embed".into());
        assert!(provider.supports_streaming());
    }

    #[test]
    fn supports_embeddings_returns_true() {
        let provider =
            OllamaProvider::new("http://localhost:11434", "test".into(), "test-embed".into());
        assert!(provider.supports_embeddings());
    }

    #[test]
    fn name_returns_ollama() {
        let provider =
            OllamaProvider::new("http://localhost:11434", "test".into(), "test-embed".into());
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn new_stores_model_and_embedding_model() {
        let provider = OllamaProvider::new(
            "http://localhost:11434",
            "mistral:7b".into(),
            "nomic-embed".into(),
        );
        assert_eq!(provider.model, "mistral:7b");
        assert_eq!(provider.embedding_model, "nomic-embed");
    }

    #[test]
    fn clone_preserves_fields() {
        let provider = OllamaProvider::new(
            "http://localhost:11434",
            "llama3".into(),
            "embed-model".into(),
        );
        let cloned = provider.clone();
        assert_eq!(cloned.model, provider.model);
        assert_eq!(cloned.embedding_model, provider.embedding_model);
    }

    #[test]
    fn debug_format() {
        let provider =
            OllamaProvider::new("http://localhost:11434", "test".into(), "test-embed".into());
        let debug = format!("{provider:?}");
        assert!(debug.contains("OllamaProvider"));
        assert!(debug.contains("test"));
    }

    #[test]
    fn parse_host_port_custom_port() {
        let (host, port) = parse_host_port("http://example.com:8080");
        assert_eq!(host, "http://example.com");
        assert_eq!(port, 8080);
    }

    #[test]
    fn parse_host_port_trailing_slash() {
        let (host, port) = parse_host_port("http://localhost:11434/");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_no_scheme() {
        let (host, port) = parse_host_port("localhost:9999");
        assert_eq!(host, "localhost");
        assert_eq!(port, 9999);
    }

    #[test]
    fn parse_host_port_invalid_port_falls_back() {
        let (host, port) = parse_host_port("http://localhost:notaport");
        assert_eq!(host, "http://localhost:notaport");
        assert_eq!(port, 11434);
    }

    #[test]
    fn convert_message_system_role() {
        let msg = Message {
            role: Role::System,
            content: "system instruction".into(),
        };
        let cm = convert_message(&msg);
        assert_eq!(cm.content, "system instruction");
    }

    #[test]
    fn convert_message_assistant_role() {
        let msg = Message {
            role: Role::Assistant,
            content: "reply text".into(),
        };
        let cm = convert_message(&msg);
        assert_eq!(cm.content, "reply text");
    }

    #[test]
    fn parse_host_port_empty_string() {
        let (host, port) = parse_host_port("");
        assert_eq!(host, "");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_only_scheme() {
        let (host, port) = parse_host_port("http://localhost");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_port_zero() {
        let (host, port) = parse_host_port("http://localhost:0");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 0);
    }

    #[test]
    fn parse_host_port_max_port() {
        let (host, port) = parse_host_port("http://localhost:65535");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 65535);
    }

    #[test]
    fn parse_host_port_port_overflow_falls_back() {
        let (host, port) = parse_host_port("http://localhost:99999");
        assert_eq!(host, "http://localhost:99999");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_ipv4() {
        let (host, port) = parse_host_port("http://192.168.1.1:8080");
        assert_eq!(host, "http://192.168.1.1");
        assert_eq!(port, 8080);
    }

    #[test]
    fn parse_host_port_multiple_trailing_slashes() {
        let (host, port) = parse_host_port("http://localhost:11434///");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 11434);
    }

    #[test]
    fn convert_message_preserves_content() {
        let msg = Message {
            role: Role::User,
            content: "multi\nline\ncontent".into(),
        };
        let cm = convert_message(&msg);
        assert_eq!(cm.content, "multi\nline\ncontent");
    }

    #[test]
    fn convert_message_empty_content() {
        let msg = Message {
            role: Role::User,
            content: String::new(),
        };
        let cm = convert_message(&msg);
        assert!(cm.content.is_empty());
    }

    #[tokio::test]
    async fn chat_with_unreachable_endpoint_errors() {
        let provider =
            OllamaProvider::new("http://127.0.0.1:1", "test-model".into(), "embed".into());
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
        }];
        let result = provider.chat(&messages).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn embed_with_unreachable_endpoint_errors() {
        let provider =
            OllamaProvider::new("http://127.0.0.1:1", "test-model".into(), "embed".into());
        let result = provider.embed("test text").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn chat_stream_with_unreachable_endpoint_errors() {
        let provider =
            OllamaProvider::new("http://127.0.0.1:1", "test-model".into(), "embed".into());
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
        }];
        let result = provider.chat_stream(&messages).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn warmup_with_unreachable_endpoint_errors() {
        let provider =
            OllamaProvider::new("http://127.0.0.1:1", "test-model".into(), "embed".into());
        let result = provider.warmup().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("warmup failed"));
    }

    #[tokio::test]
    async fn health_check_unreachable_errors() {
        let provider =
            OllamaProvider::new("http://127.0.0.1:1", "test-model".into(), "embed".into());
        let result = provider.health_check().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Ollama"));
    }

    #[test]
    fn new_with_different_urls() {
        let p1 = OllamaProvider::new("http://host1:1234", "m1".into(), "e1".into());
        let p2 = OllamaProvider::new("http://host2:5678", "m2".into(), "e2".into());
        assert_eq!(p1.model, "m1");
        assert_eq!(p2.model, "m2");
        assert_eq!(p1.embedding_model, "e1");
        assert_eq!(p2.embedding_model, "e2");
    }

    #[tokio::test]
    #[ignore = "requires running Ollama instance"]
    async fn integration_ollama_chat_stream() {
        let provider = OllamaProvider::new(
            "http://localhost:11434",
            "mistral:7b".into(),
            "qwen3-embedding".into(),
        );

        let messages = vec![Message {
            role: Role::User,
            content: "Reply with exactly: pong".into(),
        }];

        let mut stream = provider.chat_stream(&messages).await.unwrap();
        let mut chunks = Vec::new();
        let mut chunk_count = 0;

        while let Some(result) = stream.next().await {
            let chunk = result.unwrap();
            chunks.push(chunk);
            chunk_count += 1;
        }

        let full_response: String = chunks.concat();
        assert!(!full_response.is_empty());
        assert!(full_response.to_lowercase().contains("pong"));
        assert!(chunk_count >= 1);
    }

    #[tokio::test]
    #[ignore = "requires running Ollama instance"]
    async fn integration_ollama_stream_matches_chat() {
        let provider = OllamaProvider::new(
            "http://localhost:11434",
            "mistral:7b".into(),
            "qwen3-embedding".into(),
        );

        let messages = vec![Message {
            role: Role::User,
            content: "What is 2+2? Reply with just the number.".into(),
        }];

        let chat_response = provider.chat(&messages).await.unwrap();

        let mut stream = provider.chat_stream(&messages).await.unwrap();
        let mut stream_chunks = Vec::new();
        while let Some(result) = stream.next().await {
            stream_chunks.push(result.unwrap());
        }
        let stream_response: String = stream_chunks.concat();

        assert!(chat_response.contains('4'));
        assert!(stream_response.contains('4'));
    }

    #[tokio::test]
    #[ignore = "requires running Ollama instance with qwen3-embedding model"]
    async fn integration_ollama_embed() {
        let provider = OllamaProvider::new(
            "http://localhost:11434",
            "mistral:7b".into(),
            "qwen3-embedding".into(),
        );

        let embedding = provider.embed("hello world").await.unwrap();
        assert!(!embedding.is_empty());
        assert!(embedding.len() > 100);
        assert!(embedding.iter().all(|v| v.is_finite()));
    }
}
