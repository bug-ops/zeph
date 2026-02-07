use anyhow::Context;
use ollama_rs::Ollama;
use ollama_rs::generation::chat::ChatMessage;
use ollama_rs::generation::chat::request::ChatMessageRequest;
use ollama_rs::generation::embeddings::request::{EmbeddingsInput, GenerateEmbeddingsRequest};
use tokio_stream::StreamExt;

use crate::provider::{ChatStream, LlmProvider, Message, Role};

#[derive(Debug, Clone)]
pub struct OllamaProvider {
    client: Ollama,
    model: String,
    embedding_model: String,
}

impl OllamaProvider {
    #[must_use]
    pub fn new(base_url: &str, model: String, embedding_model: String) -> Self {
        let (host, port) = parse_host_port(base_url);
        Self {
            client: Ollama::new(host, port),
            model,
            embedding_model,
        }
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
}

impl LlmProvider for OllamaProvider {
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
