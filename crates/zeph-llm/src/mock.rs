// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test-only mock LLM provider.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::model_cache::RemoteModelInfo;
use crate::provider::{
    ChatResponse, ChatStream, GenerationOverrides, LlmProvider, Message, ToolDefinition,
};

#[allow(clippy::struct_excessive_bools)]
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
    /// Whether this mock reports native `tool_use` support.
    pub tool_use: bool,
    /// Pre-configured `ChatResponse` sequence returned from `chat_with_tools()`.
    /// When exhausted, falls back to `ChatResponse::Text` via `chat()`.
    tool_responses: Arc<Mutex<VecDeque<ChatResponse>>>,
    /// Records how many times `chat_with_tools()` was called.
    pub tool_call_count: Arc<Mutex<u32>>,
    /// Model list returned by `list_models_remote()`.
    pub models: Vec<RemoteModelInfo>,
    /// Optional name override for tests that require distinct provider names.
    pub name_override: Option<String>,
    /// When true, `embed()` returns `LlmError::InvalidInput` regardless of `supports_embeddings`.
    pub embed_invalid_input: bool,
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
            tool_use: false,
            tool_responses: Arc::new(Mutex::new(VecDeque::new())),
            tool_call_count: Arc::new(Mutex::new(0)),
            models: vec![],
            name_override: None,
            embed_invalid_input: false,
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

    /// Set a custom name returned by `name()`. Useful for `cost_tiers` tests that
    /// need distinct provider names without spinning up real provider instances.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name_override = Some(name.into());
        self
    }

    /// Make `embed()` return `LlmError::InvalidInput` (simulates HTTP 400 from a real provider).
    ///
    /// This enables testing the router's embed fallback loop, which must break immediately on
    /// `InvalidInput` without penalizing provider reputation.
    #[must_use]
    pub fn with_embed_invalid_input(mut self) -> Self {
        self.embed_invalid_input = true;
        self.supports_embeddings = true;
        self
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

    #[must_use]
    pub fn with_generation_overrides(self, _overrides: GenerationOverrides) -> Self {
        // No-op: mock provider ignores generation overrides.
        self
    }

    /// Set the model list returned by `list_models_remote()`.
    #[must_use]
    pub fn with_models(mut self, models: Vec<RemoteModelInfo>) -> Self {
        self.models = models;
        self
    }

    /// Enable native `tool_use` support with a pre-configured sequence of `ChatResponse`
    /// values returned from `chat_with_tools()`.
    ///
    /// Returns a shared counter that records how many times `chat_with_tools()` was called,
    /// so tests can assert the LLM was called exactly once (cache hit) or twice (cache miss).
    #[must_use]
    pub fn with_tool_use(mut self, responses: Vec<ChatResponse>) -> (Self, Arc<Mutex<u32>>) {
        self.tool_use = true;
        self.tool_responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let counter = Arc::clone(&self.tool_call_count);
        (self, counter)
    }
}

impl LlmProvider for MockProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        self.name_override.as_deref().unwrap_or("mock")
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
        if self.embed_invalid_input {
            return Err(crate::LlmError::InvalidInput {
                provider: self.name().to_owned(),
                message: "input exceeds maximum sequence length".into(),
            });
        }
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

    fn supports_tool_use(&self) -> bool {
        self.tool_use
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<ChatResponse, crate::LlmError> {
        *self.tool_call_count.lock().unwrap() += 1;
        let queued = self.tool_responses.lock().unwrap().pop_front();
        if let Some(response) = queued {
            return Ok(response);
        }
        // Fallback: delegate to chat() and wrap in Text.
        Ok(ChatResponse::Text(self.chat(messages).await?))
    }
}
