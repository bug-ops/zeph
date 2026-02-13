use std::pin::Pin;

use futures_core::Stream;
use serde::{Deserialize, Serialize};

/// Boxed stream of string chunks from an LLM provider.
pub type ChatStream = Pin<Box<dyn Stream<Item = anyhow::Result<String>> + Send>>;

/// Sender for emitting status events (retries, fallbacks) to the UI.
pub type StatusTx = tokio::sync::mpsc::UnboundedSender<String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessagePart {
    Text { text: String },
    ToolOutput { tool_name: String, body: String },
    Recall { text: String },
    CodeContext { text: String },
    Summary { text: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub parts: Vec<MessagePart>,
}

impl Message {
    #[must_use]
    pub fn from_legacy(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            parts: vec![],
        }
    }

    #[must_use]
    pub fn from_parts(role: Role, parts: Vec<MessagePart>) -> Self {
        let content = Self::flatten_parts(&parts);
        Self {
            role,
            content,
            parts,
        }
    }

    #[must_use]
    pub fn to_llm_content(&self) -> &str {
        &self.content
    }

    fn flatten_parts(parts: &[MessagePart]) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for part in parts {
            match part {
                MessagePart::Text { text }
                | MessagePart::Recall { text }
                | MessagePart::CodeContext { text }
                | MessagePart::Summary { text } => out.push_str(text),
                MessagePart::ToolOutput { tool_name, body } => {
                    let _ = write!(out, "[tool output: {tool_name}]\n```\n{body}\n```");
                }
            }
        }
        out
    }
}

pub trait LlmProvider: Send + Sync {
    /// Report the model's context window size in tokens.
    ///
    /// Returns `None` if unknown. Used for auto-budget calculation.
    fn context_window(&self) -> Option<usize> {
        None
    }

    /// Send messages to the LLM and return the assistant response.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider fails to communicate or the response is invalid.
    fn chat(&self, messages: &[Message]) -> impl Future<Output = anyhow::Result<String>> + Send;

    /// Send messages and return a stream of response chunks.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider fails to communicate or the response is invalid.
    fn chat_stream(
        &self,
        messages: &[Message],
    ) -> impl Future<Output = anyhow::Result<ChatStream>> + Send;

    /// Whether this provider supports native streaming.
    fn supports_streaming(&self) -> bool;

    /// Generate an embedding vector from text.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider does not support embeddings or the request fails.
    fn embed(&self, text: &str) -> impl Future<Output = anyhow::Result<Vec<f32>>> + Send;

    /// Whether this provider supports embedding generation.
    fn supports_embeddings(&self) -> bool;

    /// Provider name for logging and identification.
    fn name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use tokio_stream::StreamExt;

    use super::*;

    struct StubProvider {
        response: String,
    }

    impl LlmProvider for StubProvider {
        async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
            Ok(self.response.clone())
        }

        async fn chat_stream(&self, messages: &[Message]) -> anyhow::Result<ChatStream> {
            let response = self.chat(messages).await?;
            Ok(Box::pin(tokio_stream::once(Ok(response))))
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.1, 0.2, 0.3])
        }

        fn supports_embeddings(&self) -> bool {
            false
        }

        fn name(&self) -> &'static str {
            "stub"
        }
    }

    #[test]
    fn context_window_default_returns_none() {
        let provider = StubProvider {
            response: String::new(),
        };
        assert!(provider.context_window().is_none());
    }

    #[test]
    fn supports_streaming_default_returns_false() {
        let provider = StubProvider {
            response: String::new(),
        };
        assert!(!provider.supports_streaming());
    }

    #[tokio::test]
    async fn chat_stream_default_yields_single_chunk() {
        let provider = StubProvider {
            response: "hello world".into(),
        };
        let messages = vec![Message {
            role: Role::User,
            content: "test".into(),
            parts: vec![],
        }];

        let mut stream = provider.chat_stream(&messages).await.unwrap();
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk, "hello world");
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn chat_stream_default_propagates_chat_error() {
        struct FailProvider;

        impl LlmProvider for FailProvider {
            async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
                Err(anyhow::anyhow!("provider unavailable"))
            }

            async fn chat_stream(&self, messages: &[Message]) -> anyhow::Result<ChatStream> {
                let response = self.chat(messages).await?;
                Ok(Box::pin(tokio_stream::once(Ok(response))))
            }

            fn supports_streaming(&self) -> bool {
                false
            }

            async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
                Err(anyhow::anyhow!("provider unavailable"))
            }

            fn supports_embeddings(&self) -> bool {
                false
            }

            fn name(&self) -> &'static str {
                "fail"
            }
        }

        let provider = FailProvider;
        let messages = vec![Message {
            role: Role::User,
            content: "test".into(),
            parts: vec![],
        }];

        let result = provider.chat_stream(&messages).await;
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("provider unavailable"));
        }
    }

    #[tokio::test]
    async fn stub_provider_embed_returns_vector() {
        let provider = StubProvider {
            response: String::new(),
        };
        let embedding = provider.embed("test").await.unwrap();
        assert_eq!(embedding, vec![0.1, 0.2, 0.3]);
    }

    #[tokio::test]
    async fn fail_provider_embed_propagates_error() {
        struct FailProvider;

        impl LlmProvider for FailProvider {
            async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
                Err(anyhow::anyhow!("provider unavailable"))
            }

            async fn chat_stream(&self, messages: &[Message]) -> anyhow::Result<ChatStream> {
                let response = self.chat(messages).await?;
                Ok(Box::pin(tokio_stream::once(Ok(response))))
            }

            fn supports_streaming(&self) -> bool {
                false
            }

            async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
                Err(anyhow::anyhow!("embed unavailable"))
            }

            fn supports_embeddings(&self) -> bool {
                false
            }

            fn name(&self) -> &'static str {
                "fail"
            }
        }

        let provider = FailProvider;
        let result = provider.embed("test").await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("embed unavailable")
        );
    }

    #[test]
    fn role_serialization() {
        let system = Role::System;
        let user = Role::User;
        let assistant = Role::Assistant;

        assert_eq!(serde_json::to_string(&system).unwrap(), "\"system\"");
        assert_eq!(serde_json::to_string(&user).unwrap(), "\"user\"");
        assert_eq!(serde_json::to_string(&assistant).unwrap(), "\"assistant\"");
    }

    #[test]
    fn role_deserialization() {
        let system: Role = serde_json::from_str("\"system\"").unwrap();
        let user: Role = serde_json::from_str("\"user\"").unwrap();
        let assistant: Role = serde_json::from_str("\"assistant\"").unwrap();

        assert_eq!(system, Role::System);
        assert_eq!(user, Role::User);
        assert_eq!(assistant, Role::Assistant);
    }

    #[test]
    fn message_clone() {
        let msg = Message {
            role: Role::User,
            content: "test".into(),
            parts: vec![],
        };
        let cloned = msg.clone();
        assert_eq!(cloned.role, msg.role);
        assert_eq!(cloned.content, msg.content);
    }

    #[test]
    fn message_debug() {
        let msg = Message {
            role: Role::Assistant,
            content: "response".into(),
            parts: vec![],
        };
        let debug = format!("{msg:?}");
        assert!(debug.contains("Assistant"));
        assert!(debug.contains("response"));
    }

    #[test]
    fn message_serialization() {
        let msg = Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"hello\""));
    }

    #[test]
    fn message_part_serde_round_trip() {
        let parts = vec![
            MessagePart::Text {
                text: "hello".into(),
            },
            MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: "output".into(),
            },
            MessagePart::Recall {
                text: "recall".into(),
            },
            MessagePart::CodeContext {
                text: "code".into(),
            },
            MessagePart::Summary {
                text: "summary".into(),
            },
        ];
        let json = serde_json::to_string(&parts).unwrap();
        let deserialized: Vec<MessagePart> = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.len(), 5);
    }

    #[test]
    fn from_legacy_creates_empty_parts() {
        let msg = Message::from_legacy(Role::User, "hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, "hello");
        assert!(msg.parts.is_empty());
        assert_eq!(msg.to_llm_content(), "hello");
    }

    #[test]
    fn from_parts_flattens_content() {
        let msg = Message::from_parts(
            Role::System,
            vec![MessagePart::Recall {
                text: "recalled data".into(),
            }],
        );
        assert_eq!(msg.content, "recalled data");
        assert_eq!(msg.to_llm_content(), "recalled data");
        assert_eq!(msg.parts.len(), 1);
    }

    #[test]
    fn from_parts_tool_output_format() {
        let msg = Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: "hello world".into(),
            }],
        );
        assert!(msg.content.contains("[tool output: bash]"));
        assert!(msg.content.contains("hello world"));
    }

    #[test]
    fn message_deserializes_without_parts() {
        let json = r#"{"role":"user","content":"hello"}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert_eq!(msg.content, "hello");
        assert!(msg.parts.is_empty());
    }
}
