// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test-only mock LLM provider.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::model_cache::RemoteModelInfo;
use crate::provider::{
    ChatResponse, ChatStream, GenerationOverrides, LlmProvider, Message, ToolDefinition,
};

#[allow(clippy::struct_excessive_bools)] // independent boolean flags; bitflags or enum would obscure semantics without reducing complexity
#[derive(Debug, Clone)]
pub struct MockProvider {
    responses: Arc<Mutex<VecDeque<String>>>,
    pub default_response: String,
    pub embedding: Vec<f32>,
    pub supports_embeddings: bool,
    pub streaming: bool,
    pub fail_chat: bool,
    /// Whether `supports_tool_use()` reports `true`. Defaults to `true` (the trait itself now
    /// defaults to `false` per #5687, but most existing tests use `MockProvider` to simulate a
    /// tool-capable provider); set via [`MockProvider::without_tool_use`] to test tool-support
    /// escalation paths.
    pub supports_tool_use: bool,
    /// Value returned by `context_window()`. Defaults to `None`, matching the trait default;
    /// set via [`MockProvider::with_context_window`] to test context-fit escalation paths
    /// without needing a real `OllamaProvider`.
    pub context_window: Option<usize>,
    /// Whether `supports_vision()` reports `true`. Defaults to `false` (matches the trait
    /// default); set via [`MockProvider::with_vision`] to test vision-tier routing/escalation
    /// paths (spec-072) without needing a real vision-capable provider.
    pub supports_vision: bool,
    /// Milliseconds to sleep before returning a response.
    pub delay_ms: u64,
    /// Sequence of errors to return before switching to normal responses.
    /// Each call pops from the front; when empty, falls through to `responses`.
    errors: Arc<Mutex<VecDeque<crate::LlmError>>>,
    /// When set, every `chat()` call appends a clone of the messages slice here.
    recorded: Option<Arc<Mutex<Vec<Vec<Message>>>>>,
    /// Pre-configured `ChatResponse` sequence returned from `chat_with_tools()`.
    /// When exhausted, falls back to `ChatResponse::Text` via `chat()`.
    tool_responses: Arc<Mutex<VecDeque<ChatResponse>>>,
    /// Records how many times `chat_with_tools()` was called.
    pub tool_call_count: Arc<Mutex<u32>>,
    /// Model list returned by `list_models_remote()`.
    pub models: Vec<RemoteModelInfo>,
    /// Optional name override for tests that require distinct provider names.
    pub name_override: Option<String>,
    /// Optional model identifier override for tests that require `model_identifier()`
    /// to return a specific value (e.g. a reasoning-model pattern like `"o3-mini"`).
    pub model_identifier_override: Option<String>,
    /// When true, `embed()` returns `LlmError::InvalidInput` regardless of `supports_embeddings`.
    pub embed_invalid_input: bool,
    /// When true, `chat_with_tools()` returns `LlmError::InvalidInput`.
    pub tool_chat_invalid_input: bool,
    /// Tracks how many times `embed()` was called. Useful for verifying embed reuse.
    pub embed_call_count: Arc<std::sync::atomic::AtomicU64>,
    /// Milliseconds to sleep inside `embed()` before returning. Used to simulate slow providers.
    pub embed_delay_ms: u64,
    /// Fixed entropy value returned by `chat_with_extras()`. `None` returns `ChatExtras::default()`.
    pub fixed_entropy: Option<f64>,
    /// Counts currently-in-flight `chat()` calls. Updated atomically before and after the call body.
    /// Shared with the test via [`MockProvider::with_concurrency_tracking`].
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    /// High-watermark of concurrent `chat()` calls observed so far.
    peak_concurrent: Arc<std::sync::atomic::AtomicUsize>,
    /// Counts currently-in-flight `embed()` calls. Updated atomically before and after the call body.
    /// Shared with the test via [`MockProvider::with_embed_concurrency_tracking`].
    embed_in_flight: Arc<std::sync::atomic::AtomicUsize>,
    /// High-watermark of concurrent `embed()` calls observed so far.
    peak_concurrent_embed: Arc<std::sync::atomic::AtomicUsize>,
    /// Set by [`MockProvider::with_embed_concurrency_tracking`]. Gates the extra
    /// `yield_now()` in `embed()` so tests using `embed_delay_ms` without opting into
    /// tracking see unperturbed scheduling.
    embed_tracking_enabled: bool,
    /// Per-call delay sequence. Each `chat()` call pops from the front; when empty, falls back to `delay_ms`.
    per_call_delays: Arc<Mutex<VecDeque<u64>>>,
    /// Per-call delay sequence for `embed()`. Each call pops from the front; when empty, falls
    /// back to `embed_delay_ms`. See [`MockProvider::with_per_call_embed_delays`].
    per_call_embed_delays: Arc<Mutex<VecDeque<u64>>>,
    /// Captures the most recent [`GenerationOverrides`] applied via
    /// [`MockProvider::with_generation_overrides`]. Shared with the test via
    /// [`MockProvider::with_overrides_capture`] — the mock otherwise ignores overrides entirely.
    captured_overrides: Arc<Mutex<Option<GenerationOverrides>>>,
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
            supports_tool_use: true,
            context_window: None,
            supports_vision: false,
            delay_ms: 0,
            errors: Arc::new(Mutex::new(VecDeque::new())),
            recorded: None,
            tool_responses: Arc::new(Mutex::new(VecDeque::new())),
            tool_call_count: Arc::new(Mutex::new(0)),
            models: vec![],
            name_override: None,
            model_identifier_override: None,
            embed_invalid_input: false,
            tool_chat_invalid_input: false,
            embed_call_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            embed_delay_ms: 0,
            fixed_entropy: None,
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak_concurrent: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            embed_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak_concurrent_embed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            embed_tracking_enabled: false,
            per_call_delays: Arc::new(Mutex::new(VecDeque::new())),
            per_call_embed_delays: Arc::new(Mutex::new(VecDeque::new())),
            captured_overrides: Arc::new(Mutex::new(None)),
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

    /// Set a custom model identifier returned by `model_identifier()`. Useful for testing
    /// model-identifier-driven heuristics (e.g. `is_reasoning_model()`) without spinning up
    /// a real provider whose instance name differs from its model string.
    #[must_use]
    pub fn with_model_identifier(mut self, model: impl Into<String>) -> Self {
        self.model_identifier_override = Some(model.into());
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

    /// Make `chat_with_tools()` return `LlmError::InvalidInput` (simulates HTTP 400 on a
    /// malformed message sequence). Enables testing the router's tool fallback loop guard.
    #[must_use]
    pub fn with_tool_chat_invalid_input(mut self) -> Self {
        self.tool_chat_invalid_input = true;
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

    /// Enable embedding support with a fixed return vector.
    #[must_use]
    pub fn with_embedding(mut self, embedding: Vec<f32>) -> Self {
        self.embedding = embedding;
        self.supports_embeddings = true;
        self
    }

    /// Make `embed()` sleep for `ms` milliseconds before returning.
    /// Useful for testing timeout behaviour.
    #[must_use]
    pub fn with_embed_delay(mut self, ms: u64) -> Self {
        self.embed_delay_ms = ms;
        self.supports_embeddings = true;
        self
    }

    /// Assign a distinct delay (ms) to each successive `embed()` call: the Nth call sleeps
    /// `delays[N]` ms before returning; once `delays` is exhausted, falls back to
    /// `embed_delay_ms`. Mirrors [`Self::with_per_call_delays`] for `chat()`.
    ///
    /// Useful when a single turn issues multiple `embed()` calls through the same provider
    /// (e.g. skill matching's query embed followed by a separate RL re-rank query embed) and a
    /// test needs an early call to succeed quickly while a later one times out.
    #[must_use]
    pub fn with_per_call_embed_delays(mut self, delays: Vec<u64>) -> Self {
        self.per_call_embed_delays = Arc::new(Mutex::new(VecDeque::from(delays)));
        self.supports_embeddings = true;
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
    pub fn with_generation_overrides(self, overrides: GenerationOverrides) -> Self {
        // Functionally a no-op (the mock never applies overrides to a response), but records
        // the value into `captured_overrides` so tests can assert what was applied — see
        // `with_overrides_capture`.
        if let Ok(mut guard) = self.captured_overrides.lock() {
            *guard = Some(overrides);
        }
        self
    }

    /// Share `slot` as this provider's `captured_overrides` sink, so a test can read back the
    /// [`GenerationOverrides`] most recently applied via [`MockProvider::with_generation_overrides`]
    /// — including overrides applied by production code *after* the provider left the test's
    /// hands (e.g. by a `ProviderFactory` closure that rebuilds providers internally).
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::{Arc, Mutex};
    /// use zeph_llm::mock::MockProvider;
    /// use zeph_llm::provider::GenerationOverrides;
    ///
    /// let slot = Arc::new(Mutex::new(None));
    /// let provider = MockProvider::default()
    ///     .with_overrides_capture(Arc::clone(&slot))
    ///     .with_generation_overrides(GenerationOverrides {
    ///         temperature: Some(0.2),
    ///         ..Default::default()
    ///     });
    /// assert_eq!(slot.lock().unwrap().as_ref().unwrap().temperature, Some(0.2));
    /// ```
    #[must_use]
    pub fn with_overrides_capture(mut self, slot: Arc<Mutex<Option<GenerationOverrides>>>) -> Self {
        self.captured_overrides = slot;
        self
    }

    /// Set the model list returned by `list_models_remote()`.
    #[must_use]
    pub fn with_models(mut self, models: Vec<RemoteModelInfo>) -> Self {
        self.models = models;
        self
    }

    /// Set a fixed entropy value returned by `chat_with_extras()`.
    ///
    /// Required for unit tests that drive `CoE` thresholds without mocking the HTTP layer.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::mock::MockProvider;
    ///
    /// let provider = MockProvider::default().with_entropy(0.9);
    /// ```
    #[must_use]
    pub fn with_entropy(mut self, entropy: f64) -> Self {
        self.fixed_entropy = Some(entropy);
        self
    }

    /// Set per-call delay sequence for `chat()`.
    ///
    /// Each call pops from the front of the sequence; when the sequence is exhausted,
    /// `delay_ms` is used as a fallback.  This enables tests to assign distinct delays
    /// to individual calls so that futures complete in a controlled out-of-order fashion,
    /// which verifies ordering guarantees in callers that use `FuturesUnordered`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::mock::MockProvider;
    ///
    /// // First call sleeps 30 ms, second sleeps 10 ms → second completes first.
    /// let provider = MockProvider::with_responses(vec!["slow".into(), "fast".into()])
    ///     .with_per_call_delays(vec![30, 10]);
    /// ```
    #[must_use]
    pub fn with_per_call_delays(mut self, delays: Vec<u64>) -> Self {
        self.per_call_delays = Arc::new(Mutex::new(VecDeque::from(delays)));
        self
    }

    /// Enable in-flight concurrency tracking.
    ///
    /// Returns the provider and a shared atomic that holds the peak number of concurrent
    /// `chat()` calls observed across the provider's lifetime.  Each `chat()` call
    /// increments `in_flight`, records the new value into the peak if it is higher, then
    /// decrements after the body (including any `delay_ms` sleep) completes.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::mock::MockProvider;
    /// use std::sync::atomic::Ordering;
    ///
    /// let (provider, peak) = MockProvider::default().with_concurrency_tracking();
    /// // After running concurrent calls, peak.load(Ordering::SeqCst) <= expected_limit.
    /// ```
    #[must_use]
    pub fn with_concurrency_tracking(mut self) -> (Self, Arc<std::sync::atomic::AtomicUsize>) {
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        self.in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        self.peak_concurrent = Arc::clone(&peak);
        (self, peak)
    }

    /// Enable in-flight concurrency tracking for `embed()`.
    ///
    /// Returns the provider and a shared atomic that holds the peak number of concurrent
    /// `embed()` calls observed across the provider's lifetime. Mirrors
    /// [`MockProvider::with_concurrency_tracking`] but for `embed()` instead of `chat()`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::mock::MockProvider;
    /// use std::sync::atomic::Ordering;
    ///
    /// let (provider, peak) = MockProvider::default()
    ///     .with_embedding(vec![0.0; 4])
    ///     .with_embed_concurrency_tracking();
    /// // After running concurrent calls, peak.load(Ordering::SeqCst) <= expected_limit.
    /// ```
    #[must_use]
    pub fn with_embed_concurrency_tracking(
        mut self,
    ) -> (Self, Arc<std::sync::atomic::AtomicUsize>) {
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        self.embed_in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        self.peak_concurrent_embed = Arc::clone(&peak);
        self.embed_tracking_enabled = true;
        (self, peak)
    }

    /// Enable native `tool_use` support with a pre-configured sequence of `ChatResponse`
    /// values returned from `chat_with_tools()`.
    ///
    /// Returns a shared counter that records how many times `chat_with_tools()` was called,
    /// so tests can assert the LLM was called exactly once (cache hit) or twice (cache miss).
    #[must_use]
    pub fn with_tool_use(mut self, responses: Vec<ChatResponse>) -> (Self, Arc<Mutex<u32>>) {
        self.tool_responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let counter = Arc::clone(&self.tool_call_count);
        (self, counter)
    }

    /// Make `supports_tool_use()` report `false`. Used to test router/triage escalation
    /// logic that must avoid delegating tool calls to a provider without tool support.
    #[must_use]
    pub fn without_tool_use(mut self) -> Self {
        self.supports_tool_use = false;
        self
    }

    /// Set the value returned by `context_window()`. Mirrors `OllamaProvider::set_context_window`
    /// so tests can combine a specific context window with other mock behavior (e.g. no tool
    /// support) on a single provider instance.
    #[must_use]
    pub fn with_context_window(mut self, window: usize) -> Self {
        self.context_window = Some(window);
        self
    }

    /// Make `supports_vision()` report `true`. Used to test router/triage vision-tier
    /// escalation logic (spec-072) without needing a real vision-capable provider.
    #[must_use]
    pub fn with_vision(mut self) -> Self {
        self.supports_vision = true;
        self
    }
}

impl LlmProvider for MockProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        self.name_override.as_deref().unwrap_or("mock")
    }

    fn model_identifier(&self) -> &str {
        self.model_identifier_override.as_deref().unwrap_or("")
    }

    async fn chat(&self, messages: &[Message]) -> Result<String, crate::LlmError> {
        use std::sync::atomic::Ordering as AOrdering;
        let current = self.in_flight.fetch_add(1, AOrdering::SeqCst) + 1;
        self.peak_concurrent.fetch_max(current, AOrdering::SeqCst);

        let call_delay = self
            .per_call_delays
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(self.delay_ms);
        if call_delay > 0 {
            // Yield before sleeping so concurrent tasks can register in-flight before
            // any of them finish, enabling accurate peak measurement.
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(call_delay)).await;
        }
        if let Some(buf) = &self.recorded
            && let Ok(mut guard) = buf.lock()
        {
            guard.push(messages.to_vec());
        }
        let result = if self.fail_chat {
            Err(crate::LlmError::Other("mock LLM error".into()))
        } else if let Ok(mut errors) = self.errors.lock()
            && !errors.is_empty()
        {
            Err(errors.pop_front().expect("non-empty"))
        } else {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(self.default_response.clone())
            } else {
                Ok(responses.pop_front().expect("non-empty"))
            }
        };

        self.in_flight.fetch_sub(1, AOrdering::SeqCst);
        result
    }

    async fn chat_with_extras(
        &self,
        messages: &[Message],
    ) -> Result<(String, crate::provider::ChatExtras), crate::LlmError> {
        let text = self.chat(messages).await?;
        let extras = match self.fixed_entropy {
            Some(e) => crate::provider::ChatExtras::with_entropy(e),
            None => crate::provider::ChatExtras::default(),
        };
        Ok((text, extras))
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
        use std::sync::atomic::Ordering as AOrdering;
        let current = self.embed_in_flight.fetch_add(1, AOrdering::SeqCst) + 1;
        self.peak_concurrent_embed
            .fetch_max(current, AOrdering::SeqCst);

        self.embed_call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let call_delay = self
            .per_call_embed_delays
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(self.embed_delay_ms);
        if call_delay > 0 {
            if self.embed_tracking_enabled {
                // Yield before sleeping so concurrent tasks can register in-flight before
                // any of them finish, enabling accurate peak measurement. Gated on the
                // tracking opt-in so other `embed_delay_ms`-using tests see unperturbed
                // scheduling.
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(call_delay)).await;
        }
        let result = if let Ok(mut errors) = self.errors.lock()
            && !errors.is_empty()
        {
            Err(errors.pop_front().expect("non-empty"))
        } else if self.embed_invalid_input {
            Err(crate::LlmError::InvalidInput {
                provider: self.name().to_owned(),
                message: "input exceeds maximum sequence length".into(),
            })
        } else if self.supports_embeddings {
            Ok(self.embedding.clone())
        } else {
            Err(crate::LlmError::EmbedUnsupported {
                provider: "mock".into(),
            })
        };

        self.embed_in_flight.fetch_sub(1, AOrdering::SeqCst);
        result
    }

    fn supports_embeddings(&self) -> bool {
        self.supports_embeddings
    }

    fn supports_tool_use(&self) -> bool {
        self.supports_tool_use
    }

    fn supports_vision(&self) -> bool {
        self.supports_vision
    }

    fn context_window(&self) -> Option<usize> {
        self.context_window
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<ChatResponse, crate::LlmError> {
        *self.tool_call_count.lock().unwrap() += 1;
        if self.tool_chat_invalid_input {
            return Err(crate::LlmError::InvalidInput {
                provider: self.name().to_owned(),
                message: "invalid message sequence".into(),
            });
        }
        let queued = self.tool_responses.lock().unwrap().pop_front();
        if let Some(response) = queued {
            return Ok(response);
        }
        // Fallback: delegate to chat() and wrap in Text.
        Ok(ChatResponse::Text(self.chat(messages).await?))
    }
}
