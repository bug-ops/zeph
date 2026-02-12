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
pub struct Message {
    pub role: Role,
    pub content: String,
}

pub trait LlmProvider: Send + Sync {
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
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"hello\""));
    }
}
