use anyhow::Context;
use ollama_rs::Ollama;
use ollama_rs::generation::chat::request::ChatMessageRequest;
use ollama_rs::generation::chat::ChatMessage;

use crate::provider::{LlmProvider, Message, Role};

#[derive(Debug)]
pub struct OllamaProvider {
    client: Ollama,
    model: String,
}

impl OllamaProvider {
    #[must_use]
    pub fn new(base_url: &str, model: String) -> Self {
        let (host, port) = parse_host_port(base_url);
        Self {
            client: Ollama::new(host, port),
            model,
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
}
