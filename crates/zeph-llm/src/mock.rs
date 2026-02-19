//! Test-only mock LLM provider.

use std::sync::{Arc, Mutex};

use crate::provider::{ChatStream, LlmProvider, Message};

#[derive(Debug, Clone)]
pub struct MockProvider {
    responses: Arc<Mutex<Vec<String>>>,
    pub default_response: String,
    pub embedding: Vec<f32>,
    pub supports_embeddings: bool,
    pub streaming: bool,
    pub fail_chat: bool,
    /// Milliseconds to sleep before returning a response.
    pub delay_ms: u64,
}

impl Default for MockProvider {
    fn default() -> Self {
        Self {
            responses: Arc::new(Mutex::new(Vec::new())),
            default_response: "mock response".into(),
            embedding: vec![0.0; 384],
            supports_embeddings: false,
            streaming: false,
            fail_chat: false,
            delay_ms: 0,
        }
    }
}

impl MockProvider {
    #[must_use]
    pub fn with_responses(responses: Vec<String>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses)),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn failing() -> Self {
        Self {
            fail_chat: true,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_streaming(mut self) -> Self {
        self.streaming = true;
        self
    }

    #[must_use]
    pub fn with_delay(mut self, ms: u64) -> Self {
        self.delay_ms = ms;
        self
    }
}

impl LlmProvider for MockProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "mock"
    }

    async fn chat(&self, _messages: &[Message]) -> Result<String, crate::LlmError> {
        if self.delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        }
        if self.fail_chat {
            return Err(crate::LlmError::Other("mock LLM error".into()));
        }
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            Ok(self.default_response.clone())
        } else {
            Ok(responses.remove(0))
        }
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, crate::LlmError> {
        let response = self.chat(messages).await?;
        let chunks: Vec<_> = response.chars().map(|c| c.to_string()).map(Ok).collect();
        Ok(Box::pin(tokio_stream::iter(chunks)))
    }

    fn supports_streaming(&self) -> bool {
        self.streaming
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, crate::LlmError> {
        if self.supports_embeddings {
            Ok(self.embedding.clone())
        } else {
            Err(crate::LlmError::EmbedUnsupported {
                provider: "mock".into(),
            })
        }
    }

    fn supports_embeddings(&self) -> bool {
        self.supports_embeddings
    }
}
