// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test-only mock LLM provider.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::provider::{ChatStream, LlmProvider, Message};

#[derive(Debug, Clone)]
pub struct MockProvider {
    responses: Arc<Mutex<VecDeque<String>>>,
    pub default_response: String,
    pub embedding: Vec<f32>,
    pub supports_embeddings: bool,
    pub streaming: bool,
    pub fail_chat: bool,
    /// Milliseconds to sleep before returning a response.
    pub delay_ms: u64,
    /// Sequence of errors to return before switching to normal responses.
    /// Each call pops from the front; when empty, falls through to `responses`.
    errors: Arc<Mutex<VecDeque<crate::LlmError>>>,
    /// When set, every `chat()` call appends a clone of the messages slice here.
    recorded: Option<Arc<Mutex<Vec<Vec<Message>>>>>,
}

impl Default for MockProvider {
    fn default() -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::new())),
            default_response: "mock response".into(),
            embedding: vec![0.0; 384],
            supports_embeddings: false,
            streaming: false,
            fail_chat: false,
            delay_ms: 0,
            errors: Arc::new(Mutex::new(VecDeque::new())),
            recorded: None,
        }
    }
}

impl MockProvider {
    #[must_use]
    pub fn with_responses(responses: Vec<String>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
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

    /// Prepend a sequence of errors returned before normal responses.
    #[must_use]
    pub fn with_errors(mut self, errors: Vec<crate::LlmError>) -> Self {
        self.errors = Arc::new(Mutex::new(VecDeque::from(errors)));
        self
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

    /// Enable call recording. Returns the shared buffer. Each `chat()` call
    /// appends a clone of the messages slice so tests can inspect them.
    #[must_use]
    pub fn with_recording(mut self) -> (Self, Arc<Mutex<Vec<Vec<Message>>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        self.recorded = Some(Arc::clone(&buf));
        (self, buf)
    }
}

impl LlmProvider for MockProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "mock"
    }

    async fn chat(&self, messages: &[Message]) -> Result<String, crate::LlmError> {
        if self.delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        }
        if let Some(buf) = &self.recorded
            && let Ok(mut guard) = buf.lock()
        {
            guard.push(messages.to_vec());
        }
        if self.fail_chat {
            return Err(crate::LlmError::Other("mock LLM error".into()));
        }
        // Return pre-configured errors first
        if let Ok(mut errors) = self.errors.lock()
            && !errors.is_empty()
        {
            return Err(errors.pop_front().expect("non-empty"));
        }
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            Ok(self.default_response.clone())
        } else {
            Ok(responses.pop_front().expect("non-empty"))
        }
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, crate::LlmError> {
        let response = self.chat(messages).await?;
        let chunks: Vec<Result<crate::StreamChunk, crate::LlmError>> = response
            .chars()
            .map(|c| Ok(crate::StreamChunk::Content(c.to_string())))
            .collect();
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
