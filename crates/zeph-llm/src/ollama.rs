// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ollama local model backend.
//!
//! [`OllamaProvider`] connects to a running Ollama server and exposes it as an
//! [`LlmProvider`]. Both chat completion and embedding generation are supported.
//! An optional separate vision model can be configured for image-bearing messages.
//!
//! # Configuration
//!
//! ```toml
//! [[llm.providers]]
//! name = "local"
//! type = "ollama"
//! base_url = "http://localhost:11434"
//! model = "llama3.2"
//! embedding_model = "nomic-embed-text"
//! ```
//!
//! # Examples
//!
//! ```rust,no_run
//! use zeph_llm::ollama::OllamaProvider;
//! use zeph_llm::provider::{LlmProvider, Message, Role};
//!
//! # async fn run() -> Result<(), zeph_llm::LlmError> {
//! let provider = OllamaProvider::new(
//!     "http://localhost:11434",
//!     "llama3.2".into(),
//!     "nomic-embed-text".into(),
//! );
//! let messages = vec![Message::from_legacy(Role::User, "Hello!")];
//! let response = provider.chat(&messages).await?;
//! println!("{response}");
//! # Ok(())
//! # }
//! ```

use ollama_rs::Ollama;

use crate::error::LlmError;
use base64::{Engine, engine::general_purpose::STANDARD};
use ollama_rs::generation::chat::ChatMessage;
use ollama_rs::generation::chat::request::ChatMessageRequest;
use ollama_rs::generation::embeddings::request::{EmbeddingsInput, GenerateEmbeddingsRequest};
use ollama_rs::generation::images::Image as OllamaImage;
use ollama_rs::generation::tools::{ToolFunctionInfo, ToolInfo, ToolType};
use ollama_rs::models::ModelOptions;
use std::future::Future;
use tokio_stream::StreamExt;

use crate::provider::{
    ChatExtras, ChatResponse, ChatStream, GenerationOverrides, LlmProvider, Message, MessagePart,
    Role, ToolDefinition, ToolUseRequest,
};
use crate::retry::{exponential_backoff_delay, send_with_retry};
use crate::usage::UsageTracker;

/// Maximum number of retry attempts on HTTP 429/503, matching the `MAX_RETRIES` used by the
/// Claude/OpenAI/Gemini backends' `send_with_retry` (#6491).
const MAX_RETRIES: u32 = 3;

/// Serialize a chat request to a JSON body with the `stream` flag forced to `stream`.
///
/// `ChatMessageRequest::stream` is `pub(crate)` inside `ollama-rs` — only its own
/// `send_chat_messages`/`send_chat_messages_stream` methods can set it, since those are the
/// only sanctioned way to send a request. This module posts requests directly via `reqwest`
/// instead (see [`OllamaProvider::send_chat_request`]), so `ChatMessageRequest`/
/// `ChatMessageResponse` are reused only for (de)serialization shape, and the one field this
/// crate cannot set directly is patched into the serialized value here.
fn chat_request_body(
    request: &ChatMessageRequest,
    stream: bool,
) -> Result<serde_json::Value, LlmError> {
    let mut body = serde_json::to_value(request).map_err(LlmError::Json)?;
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_owned(), serde_json::Value::Bool(stream));
    }
    Ok(body)
}

/// Build a `reqwest` (workspace version, currently 0.13) HTTP client for the
/// `chat`/`chat_stream`/`chat_with_tools` request path, matching [`ollama_reqwest_client`]'s
/// timeout configuration. This is a separate client from `ollama-rs`'s internal one because
/// it is a different major `reqwest` version — `ollama-rs` depends on `reqwest` 0.12
/// (aliased `reqwest012` in this crate), while the workspace (and `retry::send_with_retry`)
/// uses 0.13 — so the two response/error types are not interchangeable. A dedicated client
/// is needed to reuse `send_with_retry` verbatim rather than going through `ollama-rs`'s
/// `send_chat_messages`/`send_chat_messages_stream`, which discard the HTTP status code on
/// non-2xx responses and so cannot support real 429/503 + `Retry-After` retry parity with
/// the Claude/OpenAI/Gemini backends (#6491).
fn ollama_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_mins(10))
        .user_agent(concat!("zeph/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("Ollama HTTP client construction must not fail")
}

/// Add `error_context` to a bare [`LlmError::Http`] (connection refused, or a transport-level
/// failure that outlasted [`send_with_transport_retry`]'s own retries — [`send_with_retry`]
/// propagates it immediately with no retry of its own, since it never produced an HTTP
/// response to inspect). Without this, a connection-level failure loses all Ollama-specific
/// context and callers that depend on the message shape to diagnose *which* provider failed
/// and why — e.g. `RouterProvider`'s fallback-exhaustion diagnostic (#5821) — regress.
/// [`LlmError::RateLimited`]/[`LlmError::Unavailable`] (produced after `send_with_retry`
/// exhausts its own retries on a real HTTP 429/503) are already self-descriptive and are
/// passed through unchanged, matching how the Claude/OpenAI/Gemini backends propagate them.
fn wrap_transport_error(err: LlmError, error_context: &str) -> LlmError {
    match err {
        LlmError::Http(e) => LlmError::Other(format!("{error_context} failed: {e}")),
        other => other,
    }
}

/// Whether a `reqwest::Error` from sending a request (as opposed to inspecting a response
/// that was actually received) represents a transient transport-level failure worth
/// retrying:
///
/// - Request timeouts (`is_timeout`) are retried.
/// - Errors while sending/receiving after a connection was established (`is_request`
///   without `is_connect`) are retried — this is the "connection reset mid-request"
///   condition #6491 names explicitly.
/// - Connect-phase failures (`is_connect`, e.g. connection refused because no Ollama server
///   is running at all) are deliberately NOT retried: they are far more likely to be a
///   permanent misconfiguration than a transient blip, and retrying them would add several
///   seconds of backoff to the common "Ollama isn't running" failure case (existing
///   `chat_with_unreachable_endpoint_errors`-style tests rely on this staying fast).
fn is_transient_transport_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || (e.is_request() && !e.is_connect())
}

/// Retry a `.send()` call up to [`MAX_RETRIES`] times on a transient transport-level failure,
/// before handing the eventual response (or a non-retryable/exhausted error) to
/// [`send_with_retry`]'s own HTTP-status retry layer.
///
/// [`send_with_retry`] only inspects a `reqwest::Response` it has already received — a
/// `.send()` call that fails before producing one (connection reset, timeout) propagates
/// immediately with no retry of its own. This wraps the request-sending closure so both
/// failure classes are covered: transport-level transience here, HTTP 429/503 status one
/// layer up. Uses the same [`exponential_backoff_delay`] schedule as `send_with_retry` does
/// for consistency across both layers.
async fn send_with_transport_retry<F, Fut>(mut f: F) -> Result<reqwest::Response, reqwest::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    let mut attempt = 0;
    loop {
        match f().await {
            Ok(response) => return Ok(response),
            Err(e) if attempt < MAX_RETRIES && is_transient_transport_error(&e) => {
                let delay = exponential_backoff_delay(attempt);
                tracing::warn!(
                    "Ollama transport-level failure, retrying in {}s ({}/{}): {e}",
                    delay.as_secs(),
                    attempt + 1,
                    MAX_RETRIES
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Parse an Ollama `/api/chat` streaming response body (NDJSON — one `ChatMessageResponse`
/// object per line) into a [`ChatStream`].
///
/// Buffers raw *bytes* across chunk boundaries rather than decoding each chunk to UTF-8 in
/// isolation: a chunk boundary can land in the middle of a multi-byte UTF-8 character or
/// mid-line, since TCP reads don't respect either boundary. Decoding per-chunk (as the
/// initial version of this function did) would silently drop the entire straddling chunk
/// when `str::from_utf8` failed on it — a real data-loss bug, not just a theoretical one,
/// inherited in spirit from `ollama-rs`'s own internal streaming implementation but now
/// freshly-written code with a straightforward fix: only decode once a complete line (up to
/// a `\n` byte) has been assembled.
fn ollama_ndjson_stream<S>(byte_stream: S) -> ChatStream
where
    S: tokio_stream::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let mapped = async_stream::stream! {
        let mut buffer: Vec<u8> = Vec::new();
        tokio::pin!(byte_stream);
        while let Some(chunk_result) = byte_stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    buffer.extend_from_slice(&chunk);
                    while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                        let line_bytes: Vec<u8> = buffer.drain(..=pos).collect();
                        let line = String::from_utf8_lossy(&line_bytes);
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<ollama_rs::generation::chat::ChatMessageResponse>(trimmed) {
                            Ok(resp) => yield Ok(crate::provider::StreamChunk::Content(resp.message.content)),
                            Err(e) => tracing::warn!("failed to deserialize Ollama stream line: {e}"),
                        }
                    }
                }
                Err(e) => {
                    yield Err(LlmError::Http(e));
                    break;
                }
            }
        }
        let tail = String::from_utf8_lossy(&buffer);
        let trimmed_tail = tail.trim();
        if !trimmed_tail.is_empty()
            && let Ok(resp) = serde_json::from_str::<ollama_rs::generation::chat::ChatMessageResponse>(trimmed_tail)
        {
            yield Ok(crate::provider::StreamChunk::Content(resp.message.content));
        }
    };

    Box::pin(mapped)
}

/// Build a reqwest 0.12 HTTP client (the version used by ollama-rs) with a 600-second hard
/// backstop timeout and a 30-second connect timeout.
fn ollama_reqwest_client() -> reqwest012::Client {
    reqwest012::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_mins(10))
        .user_agent(concat!("zeph/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("Ollama HTTP client construction must not fail")
}

/// Metadata returned by `/api/show` for the configured chat model.
#[derive(Debug)]
pub struct ModelInfo {
    /// Context window size in tokens, if reported by the server.
    pub context_length: Option<usize>,
    /// Capability tags reported by the server (e.g. `"completion"`, `"vision"`, `"tools"`).
    ///
    /// Empty when the server predates the `capabilities` field or the request failed.
    pub capabilities: Vec<String>,
}

impl ModelInfo {
    /// Whether the server-reported capabilities include `"vision"`.
    #[must_use]
    pub fn supports_vision(&self) -> bool {
        self.capabilities.iter().any(|c| c == "vision")
    }
}

/// [`LlmProvider`] backend backed by a local Ollama server.
///
/// Construct with [`OllamaProvider::new`] and optionally chain builder methods:
/// - [`with_vision_model`](Self::with_vision_model) — a separate model for image-bearing turns
/// - [`with_generation_overrides`](Self::with_generation_overrides) — temperature, top-p, top-k
/// - [`set_context_window`](Self::set_context_window) — pre-set the context window size
///
/// Call [`fetch_model_info`](Self::fetch_model_info) after construction to auto-populate
/// the context window from the server.
#[derive(Debug, Clone)]
pub struct OllamaProvider {
    client: Ollama,
    /// Dedicated `reqwest` (workspace version) client for the `chat`/`chat_stream`/
    /// `chat_with_tools` request path — see [`ollama_http_client`] for why this is a
    /// separate client from `client`'s internal one (#6491).
    http_client: reqwest::Client,
    model: String,
    embedding_model: String,
    context_window_size: Option<usize>,
    vision_model: Option<String>,
    /// Whether the configured chat `model` itself has been confirmed vision-capable via
    /// `/api/show` (see [`set_vision_capable`](Self::set_vision_capable)). Defaults to `false`
    /// — vision support is never assumed, only confirmed, so an unqueried or unreachable
    /// server fails safe to "no vision" rather than silently attaching images the model
    /// cannot process (#6377).
    vision_capable: bool,
    generation_overrides: Option<GenerationOverrides>,
    usage: UsageTracker,
    /// Name reported by [`LlmProvider::name`]. Defaults to `"ollama"`; set the TOML-configured
    /// `name` via [`with_provider_name`](Self::with_provider_name) so that router reputation
    /// tracking and provider selection can distinguish between multiple configured Ollama
    /// instances (#5859).
    provider_name: String,
}

#[allow(clippy::cast_possible_truncation)]
fn apply_generation_overrides(
    request: ChatMessageRequest,
    overrides: &GenerationOverrides,
) -> ChatMessageRequest {
    let mut opts = ModelOptions::default();
    if let Some(t) = overrides.temperature {
        tracing::debug!(temperature = t, "applying generation override: temperature");
        opts = opts.temperature(t as f32);
    }
    if let Some(tp) = overrides.top_p {
        tracing::debug!(top_p = tp, "applying generation override: top_p");
        opts = opts.top_p(tp as f32);
    }
    if let Some(tk) = overrides.top_k {
        tracing::debug!(top_k = tk, "applying generation override: top_k");
        opts = opts.top_k(tk as u32);
    }
    // frequency_penalty and presence_penalty are not supported by ollama-rs ModelOptions.
    request.options(opts)
}

impl OllamaProvider {
    /// Create a new provider targeting the given Ollama server URL.
    ///
    /// `base_url` may include a port (e.g. `"http://localhost:11434"`).
    /// `embedding_model` is used for [`LlmProvider::embed`] calls; it may be the same
    /// as `model` if the chat model also supports embeddings.
    #[must_use]
    pub fn new(base_url: &str, model: String, embedding_model: String) -> Self {
        let (host, port) = parse_host_port(base_url);
        Self {
            client: Ollama::builder()
                .host(host)
                .port(port)
                .reqwest_client(ollama_reqwest_client())
                .build(),
            http_client: ollama_http_client(),
            model,
            embedding_model,
            context_window_size: None,
            vision_model: None,
            vision_capable: false,
            generation_overrides: None,
            usage: UsageTracker::default(),
            provider_name: "ollama".to_owned(),
        }
    }

    /// Set the name reported by [`LlmProvider::name`].
    ///
    /// Populate this from the TOML-configured `name` field of the `[[llm.providers]]` entry
    /// so that router reputation tracking and generic embed-provider selection can
    /// distinguish between multiple configured Ollama instances. Without this, every
    /// `OllamaProvider` reports the same literal `"ollama"`, which corrupts per-provider
    /// availability tracking and embed routing when more than one Ollama entry is configured.
    #[must_use]
    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.provider_name = name.into();
        self
    }

    /// Override generation parameters (temperature, top-p, top-k) for this provider.
    ///
    /// Note: `frequency_penalty` and `presence_penalty` are not supported by Ollama
    /// and will be silently ignored.
    #[must_use]
    pub fn with_generation_overrides(mut self, overrides: GenerationOverrides) -> Self {
        self.generation_overrides = Some(overrides);
        self
    }

    /// Configure a separate Ollama model to use when the input contains images.
    ///
    /// When vision input is detected, the provider sends the request to this model
    /// instead of the default chat model.
    #[must_use]
    pub fn with_vision_model(mut self, model: String) -> Self {
        self.vision_model = Some(model);
        self
    }

    /// Set context window size (typically from /api/show response).
    pub fn set_context_window(&mut self, size: usize) {
        self.context_window_size = Some(size);
    }

    /// Record whether the configured chat `model` was confirmed vision-capable
    /// (typically from the `capabilities` field of an `/api/show` response, see
    /// [`fetch_model_info`](Self::fetch_model_info) and [`ModelInfo::supports_vision`]).
    pub fn set_vision_capable(&mut self, capable: bool) {
        self.vision_capable = capable;
    }

    /// Query Ollama /api/show for model metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails.
    #[tracing::instrument(name = "llm.ollama.fetch_model_info", skip_all)]
    pub async fn fetch_model_info(&self) -> Result<ModelInfo, LlmError> {
        let info = self
            .client
            .show_model_info(self.model.clone())
            .await
            .map_err(|e| LlmError::Other(format!("failed to fetch model info from Ollama: {e}")))?;

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
            capabilities: info.capabilities,
        })
    }

    /// Check if Ollama is reachable.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection to Ollama fails.
    #[tracing::instrument(name = "llm.ollama.health_check", skip_all)]
    pub async fn health_check(&self) -> Result<(), LlmError> {
        self.client
            .list_local_models()
            .await
            .map_err(|_| LlmError::Unavailable)?;
        Ok(())
    }

    /// Fetch the list of locally available models from Ollama and cache them on disk.
    ///
    /// On error the existing cache is preserved and the error is returned.
    ///
    /// # Errors
    ///
    /// Returns an error if the Ollama API request fails.
    #[tracing::instrument(name = "llm.ollama.list_models_remote", skip_all)]
    pub async fn list_models_remote(
        &self,
    ) -> Result<Vec<crate::model_cache::RemoteModelInfo>, LlmError> {
        let local_models = self
            .client
            .list_local_models()
            .await
            .map_err(|e| LlmError::Other(format!("ollama list_local_models: {e}")))?;

        let models: Vec<crate::model_cache::RemoteModelInfo> = local_models
            .into_iter()
            .map(|m| crate::model_cache::RemoteModelInfo {
                id: m.name.clone(),
                display_name: m.name,
                context_window: None,
                created_at: None,
            })
            .collect();

        let cache = crate::model_cache::ModelCache::for_slug("ollama");
        cache.save(&models).await?;
        Ok(models)
    }

    /// Send a minimal chat request to force Ollama to load the model into memory.
    ///
    /// # Errors
    ///
    /// Returns an error if the warmup request fails.
    #[tracing::instrument(name = "llm.ollama.warmup", skip_all)]
    pub async fn warmup(&self) -> Result<(), LlmError> {
        let request =
            ChatMessageRequest::new(self.model.clone(), vec![ChatMessage::user("hi".to_owned())]);
        self.client
            .send_chat_messages(request)
            .await
            .map_err(|e| LlmError::Other(format!("Ollama warmup failed: {e}")))?;
        Ok(())
    }

    /// POST a non-streaming chat request directly to `{base_url}/api/chat`, retrying on
    /// HTTP 429/503 via [`send_with_retry`] — the same helper Claude/OpenAI/Gemini use — for
    /// direct retry parity with those backends (#6491), with [`send_with_transport_retry`]
    /// layered underneath to also cover transient transport-level failures (timeout,
    /// connection reset) that never reach `send_with_retry`'s status inspection. See
    /// [`ollama_http_client`] for why this bypasses `ollama-rs`'s own `send_chat_messages`.
    ///
    /// `error_context` is prefixed to the error message on a non-2xx, non-context-length
    /// response, to distinguish the calling method (`chat` vs `chat_with_tools`) in logs.
    async fn send_chat_request(
        &self,
        request: &ChatMessageRequest,
        error_context: &str,
    ) -> Result<ollama_rs::generation::chat::ChatMessageResponse, LlmError> {
        let url = format!("{}api/chat", self.client.url_str());
        let body = chat_request_body(request, false)?;

        let response = send_with_retry(self.name(), MAX_RETRIES, None, || {
            send_with_transport_retry(|| self.http_client.post(&url).json(&body).send())
        })
        .await
        .map_err(|e| wrap_transport_error(e, error_context))?;

        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;
        if !status.is_success() {
            return Err(if crate::error::body_is_context_length_error(&text) {
                LlmError::ContextLengthExceeded
            } else {
                LlmError::Other(format!("{error_context} failed ({status}): {text}"))
            });
        }

        serde_json::from_str(&text).map_err(LlmError::Json)
    }
}

impl LlmProvider for OllamaProvider {
    fn context_window(&self) -> Option<usize> {
        self.context_window_size
    }

    /// Ollama models vary widely in vision support (e.g. `llava`/`qwen2.5vl` vs. text-only
    /// `qwen3:8b`) — unlike Claude/OpenAI/Gemini, there is no single API-wide guarantee.
    /// Reports `true` only when an explicit [`vision_model`](Self::with_vision_model) is
    /// configured (images route to that model, trusted by the operator to support them), or
    /// when the main chat `model` was confirmed vision-capable via
    /// [`fetch_model_info`](Self::fetch_model_info) + [`set_vision_capable`](Self::set_vision_capable).
    /// Fails safe to `false` — an unqueried or unreachable server never assumes vision support
    /// (#6377).
    fn supports_vision(&self) -> bool {
        self.vision_model.is_some() || self.vision_capable
    }

    fn supports_tool_use(&self) -> bool {
        true
    }

    #[tracing::instrument(
        name = "llm.chat",
        skip_all,
        fields(provider = self.name(), model = self.model_identifier())
    )]
    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        let has_images = messages
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, MessagePart::Image(_))));
        let model = if has_images {
            self.vision_model.as_deref().unwrap_or(&self.model)
        } else {
            &self.model
        };
        let ollama_messages: Vec<ChatMessage> = messages.iter().map(convert_message).collect();

        let mut request = ChatMessageRequest::new(model.to_owned(), ollama_messages);
        if let Some(ref ov) = self.generation_overrides {
            request = apply_generation_overrides(request, ov);
        }

        let response = self
            .send_chat_request(&request, "Ollama chat request")
            .await?;

        if let Some(ref fd) = response.final_data {
            self.usage.record_usage(fd.prompt_eval_count, fd.eval_count);
        }

        Ok(response.message.content)
    }

    async fn chat_with_extras(
        &self,
        messages: &[Message],
    ) -> Result<(String, ChatExtras), LlmError> {
        Ok((self.chat(messages).await?, ChatExtras::default()))
    }

    #[tracing::instrument(
        name = "llm.chat_stream",
        skip_all,
        fields(provider = self.name(), model = self.model_identifier())
    )]
    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        let has_images = messages
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, MessagePart::Image(_))));
        let model = if has_images {
            self.vision_model.as_deref().unwrap_or(&self.model)
        } else {
            &self.model
        };
        let ollama_messages: Vec<ChatMessage> = messages.iter().map(convert_message).collect();
        let mut request = ChatMessageRequest::new(model.to_owned(), ollama_messages);
        if let Some(ref ov) = self.generation_overrides {
            request = apply_generation_overrides(request, ov);
        }

        // Posted directly via `reqwest` rather than `ollama-rs`'s `send_chat_messages_stream`
        // so the initial request can retry on HTTP 429/503 via `send_with_retry` (#6491) —
        // see `send_chat_request`'s doc comment for why. The streamed body itself (NDJSON,
        // one `ChatMessageResponse` per line) is parsed manually below, mirroring what
        // `ollama-rs` does internally for `ChatMessageResponseStream`.
        let url = format!("{}api/chat", self.client.url_str());
        let body = chat_request_body(&request, true)?;

        let response = send_with_retry(self.name(), MAX_RETRIES, None, || {
            send_with_transport_retry(|| self.http_client.post(&url).json(&body).send())
        })
        .await
        .map_err(|e| wrap_transport_error(e, "Ollama streaming request"))?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.map_err(LlmError::Http)?;
            return Err(if crate::error::body_is_context_length_error(&text) {
                LlmError::ContextLengthExceeded
            } else {
                LlmError::Other(format!(
                    "Ollama streaming request failed ({status}): {text}"
                ))
            });
        }

        Ok(ollama_ndjson_stream(response.bytes_stream()))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        _stream: bool,
    ) -> serde_json::Value {
        if !tools.is_empty() {
            let ollama_tools: Vec<ToolInfo> = tools
                .iter()
                .map(|t| ToolInfo {
                    tool_type: ToolType::Function,
                    function: ToolFunctionInfo {
                        name: t.name.to_string(),
                        description: t.description.clone(),
                        parameters: serde_json::from_value(t.parameters.clone())
                            .unwrap_or_default(),
                    },
                })
                .collect();
            let ollama_messages: Vec<ChatMessage> =
                messages.iter().map(convert_message_structured).collect();
            let mut request =
                ChatMessageRequest::new(self.model.clone(), ollama_messages).tools(ollama_tools);
            if let Some(ref ov) = self.generation_overrides {
                request = apply_generation_overrides(request, ov);
            }
            return serde_json::to_value(&request)
                .unwrap_or_else(|e| serde_json::json!({ "serialization_error": e.to_string() }));
        }

        let has_images = messages
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, MessagePart::Image(_))));
        let model = if has_images {
            self.vision_model.as_deref().unwrap_or(&self.model)
        } else {
            &self.model
        };
        let ollama_messages: Vec<ChatMessage> = messages.iter().map(convert_message).collect();
        let mut request = ChatMessageRequest::new(model.to_owned(), ollama_messages);
        if let Some(ref ov) = self.generation_overrides {
            request = apply_generation_overrides(request, ov);
        }
        serde_json::to_value(&request)
            .unwrap_or_else(|e| serde_json::json!({ "serialization_error": e.to_string() }))
    }

    #[tracing::instrument(
        name = "llm.chat_with_tools",
        skip_all,
        fields(model = self.model_identifier())
    )]
    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        let ollama_tools: Vec<ToolInfo> = tools
            .iter()
            .map(|t| ToolInfo {
                tool_type: ToolType::Function,
                function: ToolFunctionInfo {
                    name: t.name.to_string(),
                    description: t.description.clone(),
                    parameters: serde_json::from_value(t.parameters.clone()).unwrap_or_default(),
                },
            })
            .collect();

        let ollama_messages: Vec<ChatMessage> =
            messages.iter().map(convert_message_structured).collect();

        let mut request =
            ChatMessageRequest::new(self.model.clone(), ollama_messages).tools(ollama_tools);
        if let Some(ref ov) = self.generation_overrides {
            request = apply_generation_overrides(request, ov);
        }

        let response = self
            .send_chat_request(&request, "Ollama chat_with_tools request")
            .await?;

        if let Some(ref fd) = response.final_data {
            self.usage.record_usage(fd.prompt_eval_count, fd.eval_count);
        }

        if response.message.tool_calls.is_empty() {
            return Ok(ChatResponse::Text(response.message.content));
        }

        let tool_calls: Vec<ToolUseRequest> = response
            .message
            .tool_calls
            .into_iter()
            .enumerate()
            .map(|(i, tc)| ToolUseRequest {
                id: format!("call_{i}"),
                name: tc.function.name.into(),
                input: tc.function.arguments,
            })
            .collect();

        let text = if response.message.content.is_empty() {
            None
        } else {
            Some(response.message.content)
        };

        Ok(ChatResponse::ToolUse {
            text,
            tool_calls,
            thinking_blocks: vec![],
        })
    }

    #[tracing::instrument(
        name = "llm.embed",
        skip_all,
        fields(provider = self.name(), model = self.embedding_model)
    )]
    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        use crate::embed::truncate_for_embed;

        let text = truncate_for_embed(text);
        let request = GenerateEmbeddingsRequest::new(
            self.embedding_model.clone(),
            EmbeddingsInput::from(text.as_ref()),
        );

        let response = self
            .client
            .generate_embeddings(request)
            .await
            .map_err(|e| LlmError::Other(format!("Ollama embedding request failed: {e}")))?;

        response
            .embeddings
            .into_iter()
            .next()
            .ok_or(LlmError::EmptyResponse {
                provider: "ollama".into(),
            })
    }

    #[tracing::instrument(
        name = "llm.embed_batch",
        skip_all,
        fields(provider = self.name(), model = self.embedding_model)
    )]
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, LlmError> {
        use crate::embed::truncate_for_embed;

        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let truncated: Vec<String> = texts
            .iter()
            .map(|t| truncate_for_embed(t).into_owned())
            .collect();

        let request = GenerateEmbeddingsRequest::new(
            self.embedding_model.clone(),
            EmbeddingsInput::from(truncated),
        );

        let response = self
            .client
            .generate_embeddings(request)
            .await
            .map_err(|e| LlmError::Other(format!("Ollama batch embedding failed: {e}")))?;

        if response.embeddings.len() != texts.len() {
            return Err(LlmError::Other(format!(
                "Ollama returned {} embeddings for {} inputs",
                response.embeddings.len(),
                texts.len()
            )));
        }

        Ok(response.embeddings)
    }

    fn supports_embeddings(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        &self.provider_name
    }

    fn model_identifier(&self) -> &str {
        &self.model
    }

    fn last_usage(&self) -> Option<(u64, u64)> {
        self.usage.last_usage()
    }
}

/// Convert a message for tool-aware requests. Handles `ToolUse` and `ToolResult` parts.
fn convert_message_structured(msg: &Message) -> ChatMessage {
    // If the message contains ToolResult parts, emit them as role:tool messages.
    // ollama-rs represents tool results as a single ChatMessage with role Tool.
    // We concatenate all tool result contents (Ollama expects one message per turn).
    let tool_results: Vec<&MessagePart> = msg
        .parts
        .iter()
        .filter(|p| matches!(p, MessagePart::ToolResult { .. }))
        .collect();
    if !tool_results.is_empty() {
        let content = tool_results
            .iter()
            .filter_map(|p| {
                if let MessagePart::ToolResult { content, .. } = p {
                    Some(content.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let tool_message = ChatMessage::tool(content);
        // Sibling Image parts (e.g. from MCP media passthrough) must not be dropped:
        // ollama-rs does not role-restrict ChatMessage.images, so attach them here too.
        let images = extract_images(msg);
        return if images.is_empty() {
            tool_message
        } else {
            tool_message.with_images(images)
        };
    }
    convert_message(msg)
}

/// Collect all `MessagePart::Image` siblings from a message, base64-encoded for `ollama-rs`.
fn extract_images(msg: &Message) -> Vec<OllamaImage> {
    msg.parts
        .iter()
        .filter_map(|p| match p {
            MessagePart::Image(img) => Some(OllamaImage::from_base64(STANDARD.encode(&img.data))),
            _ => None,
        })
        .collect()
}

fn convert_message(msg: &Message) -> ChatMessage {
    let images = extract_images(msg);

    let text = msg.to_llm_content().to_string();

    match msg.role {
        Role::System => ChatMessage::system(text),
        Role::Assistant => ChatMessage::assistant(text),
        Role::User => {
            if images.is_empty() {
                ChatMessage::user(text)
            } else {
                ChatMessage::user(text).with_images(images)
            }
        }
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

fn parse_host_port(base_url: &str) -> (String, u16) {
    // Only use the URL parser for strings that start with a proper scheme
    let has_scheme = base_url.starts_with("http://") || base_url.starts_with("https://");
    if has_scheme && let Ok(parsed) = url::Url::parse(base_url) {
        let port = parsed.port().unwrap_or(11434);
        let scheme = parsed.scheme();
        let host_part = match parsed.host() {
            Some(url::Host::Ipv6(addr)) => format!("[{addr}]"),
            _ => parsed.host_str().unwrap_or("localhost").to_string(),
        };
        return (format!("{scheme}://{host_part}"), port);
    }
    // Fallback for bare "host:port" strings that have no scheme (e.g. "localhost:11434").
    // url::Url::parse() treats the part before the first ':' as a scheme in that case,
    // so the scheme-gated branch above is intentionally skipped for such inputs.
    let trimmed = base_url.trim_end_matches('/');
    if let Some(colon_pos) = trimmed.rfind(':') {
        let port_str = &trimmed[colon_pos + 1..];
        if let Ok(port) = port_str.parse::<u16>() {
            return (trimmed[..colon_pos].to_string(), port);
        }
    }
    (trimmed.to_string(), 11434)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ImageData;
    use crate::provider::MessageMetadata;
    use std::assert_matches;

    fn ollama_chat_model() -> String {
        std::env::var("OLLAMA_CHAT_MODEL").unwrap_or_else(|_| "qwen3:8b".into())
    }

    fn ollama_embed_model() -> String {
        std::env::var("OLLAMA_EMBED_MODEL").unwrap_or_else(|_| "qwen3-embedding".into())
    }

    // --- #4729: Ollama HTTP client smoke test ---

    #[test]
    fn ollama_reqwest_client_builds_without_panicking() {
        // Verify that constructing the timeout-configured reqwest client does not panic.
        let _client = ollama_reqwest_client();
    }

    #[test]
    fn context_length_error_keywords_are_detected() {
        // Verify the helper used at each chat/stream/tools call site works for Ollama error strings.
        assert!(crate::error::body_is_context_length_error(
            "context length exceeded for this model"
        ));
        assert!(crate::error::body_is_context_length_error(
            "maximum number of tokens is 4096"
        ));
        assert!(crate::error::body_is_context_length_error(
            "prompt is too long"
        ));
        assert!(!crate::error::body_is_context_length_error(
            "connection refused"
        ));
    }

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
    fn parse_host_port_strips_v1_path() {
        let (host, port) = parse_host_port("http://localhost:11434/v1");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_strips_v1_trailing_slash() {
        let (host, port) = parse_host_port("http://localhost:11434/v1/");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_ipv4_with_path() {
        let (host, port) = parse_host_port("http://192.168.1.100:11434/v1");
        assert_eq!(host, "http://192.168.1.100");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_ipv6_with_path() {
        let (host, port) = parse_host_port("http://[::1]:11434/v1");
        assert_eq!(host, "http://[::1]");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_https_with_path() {
        let (host, port) = parse_host_port("https://host:11434/v1");
        assert_eq!(host, "https://host");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_ipv6_no_port() {
        let (host, port) = parse_host_port("http://[::1]/v1");
        assert_eq!(host, "http://[::1]");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_bare_host_colon_port() {
        let (host, port) = parse_host_port("localhost:11434");
        assert_eq!(host, "localhost");
        assert_eq!(port, 11434);
    }

    #[test]
    fn convert_message_roles() {
        let msg = Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        };
        let cm = convert_message(&msg);
        assert_eq!(cm.content, "hello");
    }

    #[test]
    fn last_usage_initially_none() {
        let provider =
            OllamaProvider::new("http://localhost:11434", "test".into(), "test-embed".into());
        assert!(provider.last_usage().is_none());
    }

    #[test]
    fn clone_resets_last_usage() {
        let provider =
            OllamaProvider::new("http://localhost:11434", "test".into(), "test-embed".into());
        provider.usage.record_usage(100, 50);
        assert!(provider.last_usage().is_some());
        let cloned = provider.clone();
        assert!(cloned.last_usage().is_none());
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
    fn with_provider_name_overrides_name() {
        let provider =
            OllamaProvider::new("http://localhost:11434", "test".into(), "test-embed".into())
                .with_provider_name("local-chat");
        assert_eq!(provider.name(), "local-chat");
    }

    #[test]
    fn with_provider_name_distinguishes_multiple_instances() {
        let chat = OllamaProvider::new("http://localhost:11434", "chat-model".into(), "e".into())
            .with_provider_name("chat");
        let embed = OllamaProvider::new("http://localhost:11434", "m".into(), "embed-model".into())
            .with_provider_name("embedder");
        assert_ne!(chat.name(), embed.name());
        assert_eq!(chat.name(), "chat");
        assert_eq!(embed.name(), "embedder");
    }

    #[test]
    fn new_stores_model_and_embedding_model() {
        let provider = OllamaProvider::new(
            "http://localhost:11434",
            "qwen3:8b".into(),
            "nomic-embed".into(),
        );
        assert_eq!(provider.model, "qwen3:8b");
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
            parts: vec![],
            metadata: MessageMetadata::default(),
        };
        let cm = convert_message(&msg);
        assert_eq!(cm.content, "system instruction");
    }

    #[test]
    fn convert_message_assistant_role() {
        let msg = Message {
            role: Role::Assistant,
            content: "reply text".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
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
            parts: vec![],
            metadata: MessageMetadata::default(),
        };
        let cm = convert_message(&msg);
        assert_eq!(cm.content, "multi\nline\ncontent");
    }

    #[test]
    fn convert_message_empty_content() {
        let msg = Message {
            role: Role::User,
            content: String::new(),
            parts: vec![],
            metadata: MessageMetadata::default(),
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
            parts: vec![],
            metadata: MessageMetadata::default(),
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
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let result = provider.chat_stream(&messages).await;
        assert!(result.is_err());
    }

    // --- #6491: Ollama retry/backoff with real HTTP-status parity ---
    //
    // These exercise `chat()`/`chat_stream()` end-to-end against a `wiremock` `/api/chat`
    // server (same mechanism `openai`/`claude` use for their `send_with_retry` coverage,
    // e.g. `openai::tests::chat_429_rate_limit_propagates`) so the real
    // `retry::send_with_retry` codepath (status/`Retry-After` inspection) is what's under
    // test, not a reimplementation of it.

    fn chat_test_messages() -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: "hi".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }]
    }

    #[tokio::test]
    async fn chat_retries_after_429_then_succeeds() {
        use crate::testing::{ollama_chat_response, ollama_rate_limit_response};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_rate_limit_response())
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_chat_response("pong"))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(&server.uri(), "test-model".into(), "embed".into());

        match provider.chat(&chat_test_messages()).await {
            Ok(text) => assert_eq!(text, "pong"),
            Err(e) => panic!("expected success after one 429 retry, got error: {e}"),
        }
    }

    #[tokio::test]
    async fn chat_exhausts_retries_on_persistent_503() {
        use crate::testing::ollama_unavailable_response;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_unavailable_response())
            .expect(u64::from(MAX_RETRIES) + 1)
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(&server.uri(), "test-model".into(), "embed".into());

        let result = provider.chat(&chat_test_messages()).await;
        assert!(
            matches!(result, Err(LlmError::Unavailable)),
            "expected Unavailable after exhausting retries, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn chat_context_length_error_is_not_retried() {
        use crate::testing::ollama_context_length_response;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        // `.expect(1)`: if the context-length error were mistakenly retried, the mock's call
        // count would exceed this expectation, turning this into a clearly failing test
        // rather than a silently-passing one.
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_context_length_response())
            .expect(1)
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(&server.uri(), "test-model".into(), "embed".into());

        let result = provider.chat(&chat_test_messages()).await;
        assert!(
            matches!(result, Err(LlmError::ContextLengthExceeded)),
            "expected ContextLengthExceeded without retry, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn chat_stream_retries_after_503_then_succeeds() {
        use crate::testing::{ollama_chat_response, ollama_unavailable_response};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_unavailable_response())
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_chat_response("pong"))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(&server.uri(), "test-model".into(), "embed".into());

        let mut stream = provider
            .chat_stream(&chat_test_messages())
            .await
            .expect("stream request should succeed after one 503 retry");
        let mut full_response = String::new();
        while let Some(item) = stream.next().await {
            if let crate::provider::StreamChunk::Content(text) = item.expect("stream chunk") {
                full_response.push_str(&text);
            }
        }
        assert_eq!(full_response, "pong");
    }

    #[tokio::test]
    async fn chat_stream_context_length_error_is_not_retried() {
        use crate::testing::ollama_context_length_response;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_context_length_response())
            .expect(1)
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(&server.uri(), "test-model".into(), "embed".into());

        match provider.chat_stream(&chat_test_messages()).await {
            Err(LlmError::ContextLengthExceeded) => {}
            Err(e) => panic!("expected ContextLengthExceeded without retry, got error: {e}"),
            Ok(_) => panic!("expected ContextLengthExceeded without retry, got a stream"),
        }
    }

    #[tokio::test]
    async fn chat_with_tools_retries_after_429_then_succeeds() {
        use crate::testing::{ollama_chat_response, ollama_rate_limit_response};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_rate_limit_response())
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_chat_response("pong"))
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(&server.uri(), "test-model".into(), "embed".into());
        let tools = vec![ToolDefinition {
            name: "noop".into(),
            description: "no-op tool".into(),
            parameters: serde_json::json!({"type": "object"}),
            output_schema: None,
        }];

        match provider
            .chat_with_tools(&chat_test_messages(), &tools)
            .await
        {
            Ok(ChatResponse::Text(text)) => assert_eq!(text, "pong"),
            Ok(other) => panic!("expected ChatResponse::Text(\"pong\"), got: {other:?}"),
            Err(e) => panic!("expected success after one 429 retry, got error: {e}"),
        }
    }

    #[test]
    fn chat_request_body_sets_stream_flag() {
        let request =
            ChatMessageRequest::new("test-model".into(), vec![ChatMessage::user("hi".into())]);

        let non_streaming = chat_request_body(&request, false).unwrap();
        assert_eq!(non_streaming["stream"], false);

        let streaming = chat_request_body(&request, true).unwrap();
        assert_eq!(streaming["stream"], true);
    }

    #[tokio::test]
    async fn ollama_ndjson_stream_reassembles_utf8_split_across_chunk_boundary() {
        let body = serde_json::json!({
            "model": "test-model",
            "created_at": "2024-01-01T00:00:00Z",
            "message": { "role": "assistant", "content": "café" },
            "done": true
        });
        let mut line = serde_json::to_vec(&body).unwrap();
        line.push(b'\n');
        // 'é' (U+00E9) encodes as the two bytes 0xC3 0xA9 in UTF-8 — split the chunk
        // boundary between them to reproduce a network read landing mid-character. Decoding
        // each chunk to UTF-8 independently (the pre-fix behavior) would fail to decode the
        // first chunk (it ends on an incomplete sequence) and silently drop it.
        let split_at = line
            .iter()
            .position(|&b| b == 0xC3)
            .expect("body contains an encoded 'é'")
            + 1;
        let chunk1 = bytes::Bytes::copy_from_slice(&line[..split_at]);
        let chunk2 = bytes::Bytes::copy_from_slice(&line[split_at..]);

        let source: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![Ok(chunk1), Ok(chunk2)];
        let mut stream = ollama_ndjson_stream(tokio_stream::iter(source));

        let mut full_response = String::new();
        while let Some(item) = stream.next().await {
            if let crate::provider::StreamChunk::Content(text) = item.expect("stream chunk") {
                full_response.push_str(&text);
            }
        }
        assert_eq!(full_response, "café");
    }

    #[tokio::test]
    async fn ollama_ndjson_stream_yields_multiple_lines_from_one_chunk() {
        let mut combined = Vec::new();
        for content in ["hello ", "world"] {
            let body = serde_json::json!({
                "model": "test-model",
                "created_at": "2024-01-01T00:00:00Z",
                "message": { "role": "assistant", "content": content },
                "done": true
            });
            combined.extend(serde_json::to_vec(&body).unwrap());
            combined.push(b'\n');
        }

        let source: Vec<Result<bytes::Bytes, reqwest::Error>> =
            vec![Ok(bytes::Bytes::from(combined))];
        let mut stream = ollama_ndjson_stream(tokio_stream::iter(source));

        let mut full_response = String::new();
        while let Some(item) = stream.next().await {
            if let crate::provider::StreamChunk::Content(text) = item.expect("stream chunk") {
                full_response.push_str(&text);
            }
        }
        assert_eq!(full_response, "hello world");
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
        assert_matches!(result, Err(crate::LlmError::Unavailable));
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
            ollama_chat_model(),
            ollama_embed_model(),
        );

        let messages = vec![Message {
            role: Role::User,
            content: "Reply with exactly: pong".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let mut stream = provider.chat_stream(&messages).await.unwrap();
        let mut chunk_count = 0;

        let mut full_response = String::new();
        while let Some(result) = stream.next().await {
            if let crate::provider::StreamChunk::Content(text) = result.unwrap() {
                full_response.push_str(&text);
            }
            chunk_count += 1;
        }

        assert!(!full_response.is_empty());
        assert!(full_response.to_lowercase().contains("pong"));
        assert!(chunk_count >= 1);
    }

    #[tokio::test]
    #[ignore = "requires running Ollama instance"]
    async fn integration_ollama_stream_matches_chat() {
        let provider = OllamaProvider::new(
            "http://localhost:11434",
            ollama_chat_model(),
            ollama_embed_model(),
        );

        let messages = vec![Message {
            role: Role::User,
            content: "What is 2+2? Reply with just the number.".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let chat_response = provider.chat(&messages).await.unwrap();

        let mut stream = provider.chat_stream(&messages).await.unwrap();
        let mut stream_response = String::new();
        while let Some(result) = stream.next().await {
            if let crate::provider::StreamChunk::Content(text) = result.unwrap() {
                stream_response.push_str(&text);
            }
        }

        assert!(chat_response.contains('4'));
        assert!(stream_response.contains('4'));
    }

    #[tokio::test]
    #[ignore = "requires running Ollama instance"]
    async fn integration_ollama_embed() {
        let provider = OllamaProvider::new(
            "http://localhost:11434",
            ollama_chat_model(),
            ollama_embed_model(),
        );

        let embedding = provider.embed("hello world").await.unwrap();
        assert!(!embedding.is_empty());
        assert!(embedding.len() > 100);
        assert!(embedding.iter().all(|v| v.is_finite()));
    }

    #[tokio::test]
    #[ignore = "requires running Ollama instance"]
    async fn integration_ollama_chat_with_tools() {
        // Covers the raw-HTTP `send_chat_request` path (#6491) with a real Ollama round-trip,
        // per the LLM serialization gate for changes touching request/response (de)serialization.
        let provider = OllamaProvider::new(
            "http://localhost:11434",
            ollama_chat_model(),
            ollama_embed_model(),
        );

        let messages = vec![Message {
            role: Role::User,
            content: "What's the weather in Paris? Use the get_weather tool.".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let tools = vec![ToolDefinition {
            name: "get_weather".into(),
            description: "Get the current weather for a city".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            }),
            output_schema: None,
        }];

        let response = provider
            .chat_with_tools(&messages, &tools)
            .await
            .expect("chat_with_tools should succeed against a live Ollama server");

        match response {
            ChatResponse::ToolUse { tool_calls, .. } => {
                assert!(!tool_calls.is_empty());
                assert_eq!(tool_calls[0].name.as_str(), "get_weather");
            }
            ChatResponse::Text(text) => {
                panic!("expected a tool call, model replied with text instead: {text}");
            }
        }
    }

    #[test]
    fn with_vision_model_sets_field() {
        let provider = OllamaProvider::new("http://localhost:11434", "main".into(), "embed".into())
            .with_vision_model("llava:13b".into());
        assert_eq!(provider.vision_model.as_deref(), Some("llava:13b"));
    }

    #[test]
    fn with_vision_model_builder_returns_self() {
        let provider = OllamaProvider::new("http://localhost:11434", "main".into(), "embed".into())
            .with_vision_model("llava:7b".into());
        assert_eq!(provider.model, "main");
        assert_eq!(provider.vision_model.as_deref(), Some("llava:7b"));
    }

    #[test]
    fn convert_message_text_only_has_no_images() {
        let msg = Message::from_legacy(Role::User, "hello");
        let chat_msg = convert_message(&msg);
        // No images attached — role should be User, content non-empty
        assert_eq!(
            chat_msg.role,
            ollama_rs::generation::chat::MessageRole::User
        );
        assert!(!chat_msg.content.is_empty());
    }

    #[test]
    fn convert_message_with_image_encodes_base64() {
        use base64::{Engine, engine::general_purpose::STANDARD};

        let data = vec![0xFFu8, 0xD8, 0xFF];
        let msg = Message::from_parts(
            Role::User,
            vec![
                MessagePart::Text {
                    text: "describe".into(),
                },
                MessagePart::Image(Box::new(ImageData {
                    data: data.clone(),
                    mime_type: "image/jpeg".into(),
                })),
            ],
        );
        let chat_msg = convert_message(&msg);
        let images = chat_msg.images.unwrap_or_default();
        assert_eq!(images.len(), 1);
        // OllamaImage stores the base64 string internally — verify via Debug/format
        let img_debug = format!("{:?}", images[0]);
        assert!(img_debug.contains(&STANDARD.encode(&data)));
    }

    #[test]
    fn model_selection_uses_vision_model_when_images_present() {
        let provider = OllamaProvider::new("http://localhost:11434", "main".into(), "embed".into())
            .with_vision_model("llava:13b".into());

        let has_images = true;
        let selected = if has_images {
            provider.vision_model.as_deref().unwrap_or(&provider.model)
        } else {
            &provider.model
        };
        assert_eq!(selected, "llava:13b");

        let has_images = false;
        let selected = if has_images {
            provider.vision_model.as_deref().unwrap_or(&provider.model)
        } else {
            &provider.model
        };
        assert_eq!(selected, "main");
    }

    #[test]
    fn model_selection_falls_back_to_main_without_vision_model() {
        let provider = OllamaProvider::new("http://localhost:11434", "main".into(), "embed".into());
        let selected = provider.vision_model.as_deref().unwrap_or(&provider.model);
        assert_eq!(selected, "main");
    }

    // --- #6377: supports_vision must not be hardcoded true ---

    #[test]
    fn supports_vision_false_by_default() {
        // A freshly constructed provider has neither an explicit vision_model nor a
        // confirmed-capable main model — must fail safe to false, not assume vision support.
        let provider =
            OllamaProvider::new("http://localhost:11434", "qwen3:8b".into(), "embed".into());
        assert!(!provider.supports_vision());
    }

    #[test]
    fn supports_vision_true_with_explicit_vision_model() {
        // An operator-configured vision_model is a trusted opt-in: images route to that
        // model, so supports_vision must report true regardless of main model capability.
        let provider =
            OllamaProvider::new("http://localhost:11434", "qwen3:8b".into(), "embed".into())
                .with_vision_model("llava:13b".into());
        assert!(provider.supports_vision());
    }

    #[test]
    fn supports_vision_true_after_set_vision_capable() {
        let mut provider =
            OllamaProvider::new("http://localhost:11434", "llava:13b".into(), "embed".into());
        assert!(!provider.supports_vision());
        provider.set_vision_capable(true);
        assert!(provider.supports_vision());
    }

    #[test]
    fn set_vision_capable_false_keeps_supports_vision_false() {
        let mut provider =
            OllamaProvider::new("http://localhost:11434", "qwen3:8b".into(), "embed".into());
        provider.set_vision_capable(false);
        assert!(!provider.supports_vision());
    }

    #[test]
    fn model_info_supports_vision_true_when_capability_present() {
        let info = ModelInfo {
            context_length: Some(4096),
            capabilities: vec!["completion".into(), "vision".into(), "tools".into()],
        };
        assert!(info.supports_vision());
    }

    #[test]
    fn model_info_supports_vision_false_when_capability_absent() {
        let info = ModelInfo {
            context_length: Some(4096),
            capabilities: vec!["completion".into(), "tools".into()],
        };
        assert!(!info.supports_vision());
    }

    #[test]
    fn model_info_supports_vision_false_when_capabilities_empty() {
        let info = ModelInfo {
            context_length: None,
            capabilities: vec![],
        };
        assert!(!info.supports_vision());
    }

    #[test]
    fn convert_message_structured_tool_result_emits_tool_role() {
        let msg = Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "id1".into(),
                content: "file list".into(),
                is_error: false,
            }],
        );
        let chat_msg = convert_message_structured(&msg);
        assert_eq!(
            chat_msg.role,
            ollama_rs::generation::chat::MessageRole::Tool
        );
        assert_eq!(chat_msg.content, "file list");
        assert!(chat_msg.images.is_none());
    }

    #[test]
    fn convert_message_structured_tool_result_with_image_sibling_attaches_image() {
        use base64::{Engine, engine::general_purpose::STANDARD};

        let data = vec![0xFFu8, 0xD8, 0xFF];
        let msg = Message::from_parts(
            Role::User,
            vec![
                MessagePart::ToolResult {
                    tool_use_id: "id1".into(),
                    content: "file list".into(),
                    is_error: false,
                },
                MessagePart::Image(Box::new(ImageData {
                    data: data.clone(),
                    mime_type: "image/jpeg".into(),
                })),
            ],
        );
        let chat_msg = convert_message_structured(&msg);
        assert_eq!(
            chat_msg.role,
            ollama_rs::generation::chat::MessageRole::Tool
        );
        assert_eq!(chat_msg.content, "file list");
        let images = chat_msg.images.expect("image sibling must be attached");
        assert_eq!(images.len(), 1);
        let img_debug = format!("{:?}", images[0]);
        assert!(img_debug.contains(&STANDARD.encode(&data)));
    }

    #[test]
    fn convert_message_structured_multiple_tool_results_joined() {
        let msg = Message::from_parts(
            Role::User,
            vec![
                MessagePart::ToolResult {
                    tool_use_id: "id1".into(),
                    content: "result_a".into(),
                    is_error: false,
                },
                MessagePart::ToolResult {
                    tool_use_id: "id2".into(),
                    content: "result_b".into(),
                    is_error: false,
                },
            ],
        );
        let chat_msg = convert_message_structured(&msg);
        assert_eq!(
            chat_msg.role,
            ollama_rs::generation::chat::MessageRole::Tool
        );
        assert!(chat_msg.content.contains("result_a"));
        assert!(chat_msg.content.contains("result_b"));
    }

    #[test]
    fn convert_message_structured_no_tool_results_delegates_to_convert_message() {
        let msg = Message::from_legacy(Role::Assistant, "response");
        let chat_msg = convert_message_structured(&msg);
        assert_eq!(
            chat_msg.role,
            ollama_rs::generation::chat::MessageRole::Assistant
        );
        assert_eq!(chat_msg.content, "response");
    }

    #[test]
    fn with_generation_overrides_stores_overrides() {
        let provider = OllamaProvider::new("http://127.0.0.1:11434", "m".into(), "e".into());
        assert!(provider.generation_overrides.is_none());
        let overrides = GenerationOverrides {
            temperature: Some(0.5),
            top_p: Some(0.9),
            top_k: Some(40),
            frequency_penalty: None,
            presence_penalty: None,
        };
        let patched = provider.with_generation_overrides(overrides);
        let ov = patched
            .generation_overrides
            .as_ref()
            .expect("overrides set");
        assert_eq!(ov.temperature, Some(0.5));
        assert_eq!(ov.top_p, Some(0.9));
        assert_eq!(ov.top_k, Some(40));
    }

    #[tokio::test]
    async fn chat_with_tools_unreachable_endpoint_errors() {
        let provider =
            OllamaProvider::new("http://127.0.0.1:1", "test-model".into(), "embed".into());
        let messages = vec![Message::from_legacy(Role::User, "hello")];
        let tools = vec![ToolDefinition {
            name: "test_tool".into(),
            description: "A test tool".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            output_schema: None,
        }];
        let result: Result<_, _> = provider.chat_with_tools(&messages, &tools).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn embed_batch_empty_returns_empty_without_network() {
        // Use an unreachable endpoint — empty input must return immediately without HTTP call.
        let provider =
            OllamaProvider::new("http://127.0.0.1:1", "test-model".into(), "embed".into());
        let result = provider.embed_batch(&[]).await.unwrap();
        assert!(result.is_empty());
    }
}
