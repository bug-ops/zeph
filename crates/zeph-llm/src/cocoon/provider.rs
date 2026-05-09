// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`CocoonProvider`]: an OpenAI-compatible LLM backend routed through the Cocoon
//! confidential compute sidecar.
//!
//! Request bodies are constructed by an inner [`OpenAiProvider`] (the sidecar
//! accepts the standard `OpenAI` wire format), then forwarded via [`CocoonClient`]
//! to the localhost sidecar which handles RA-TLS, proxy selection, and TEE routing.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use tracing::Instrument as _;

use crate::cocoon::client::CocoonClient;
use crate::embed::truncate_for_embed;
use crate::error::LlmError;
use crate::openai::OpenAiProvider;
use crate::provider::{ChatResponse, ChatStream, LlmProvider, Message, StatusTx, ToolDefinition};
use crate::sse::openai_sse_to_stream;
use crate::usage::UsageTracker;

// ── Wire types (local copies, same pattern as gonka/provider.rs) ─────────────

#[derive(Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    input: &'a str,
    model: &'a str,
}

#[derive(Serialize)]
struct EmbeddingBatchRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
}

// ── CocoonProvider ────────────────────────────────────────────────────────────

/// LLM provider that routes requests through the Cocoon confidential compute network.
///
/// Request bodies are constructed by an inner [`OpenAiProvider`] (the Cocoon sidecar
/// accepts the OpenAI-compatible wire format). [`CocoonClient`] handles the HTTP
/// transport to the localhost sidecar. The sidecar manages RA-TLS attestation,
/// proxy selection, and TON payments transparently.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use std::time::Duration;
/// use zeph_llm::cocoon::{CocoonClient, CocoonProvider};
///
/// let client = Arc::new(CocoonClient::new(
///     "http://localhost:10000",
///     None,
///     Duration::from_secs(30),
/// ));
/// let provider = CocoonProvider::new("Qwen/Qwen3-0.6B", 4096, None, client);
/// ```
pub struct CocoonProvider {
    /// Used only for body construction and capability flags — never called directly.
    inner: OpenAiProvider,
    /// Shared HTTP transport to the Cocoon sidecar.
    client: Arc<CocoonClient>,
    /// Embedding model name, separate from the chat model stored in `inner`.
    embedding_model: Option<String>,
    usage: UsageTracker,
    /// Optional TUI status sender.
    pub(crate) status_tx: Option<StatusTx>,
}

impl std::fmt::Debug for CocoonProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CocoonProvider")
            .field("model", &self.inner.model_identifier())
            .field("embedding_model", &self.embedding_model)
            .finish_non_exhaustive()
    }
}

impl Clone for CocoonProvider {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            client: Arc::clone(&self.client),
            embedding_model: self.embedding_model.clone(),
            usage: UsageTracker::default(),
            status_tx: self.status_tx.clone(),
        }
    }
}

impl CocoonProvider {
    /// Construct a new `CocoonProvider`.
    ///
    /// - `model` — chat model name for request bodies (e.g. `"Qwen/Qwen3-0.6B"`).
    /// - `max_tokens` — upper bound on completion tokens.
    /// - `embedding_model` — if `Some`, enables embedding via `/v1/embeddings`.
    /// - `client` — shared `CocoonClient` (also used by doctor/TUI commands).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use std::time::Duration;
    /// use zeph_llm::cocoon::{CocoonClient, CocoonProvider};
    ///
    /// let client = Arc::new(CocoonClient::new(
    ///     "http://localhost:10000", None, Duration::from_secs(30),
    /// ));
    /// let provider = CocoonProvider::new("Qwen/Qwen3-0.6B", 4096, None, client);
    /// ```
    #[must_use]
    pub fn new(
        model: impl Into<String>,
        max_tokens: u32,
        embedding_model: Option<String>,
        client: Arc<CocoonClient>,
    ) -> Self {
        let model = model.into();
        let inner = OpenAiProvider::new(
            String::new(),
            String::new(),
            model,
            max_tokens,
            embedding_model.clone(),
            None,
        );
        Self {
            inner,
            client,
            embedding_model,
            usage: UsageTracker::default(),
            status_tx: None,
        }
    }

    /// Set the TUI status sender; lifecycle events are forwarded to the TUI when set.
    pub fn set_status_tx(&mut self, tx: StatusTx) {
        self.status_tx = Some(tx);
    }

    /// Return a clone of this provider with generation parameter overrides applied.
    #[must_use]
    pub fn with_generation_overrides(
        mut self,
        overrides: crate::provider::GenerationOverrides,
    ) -> Self {
        self.inner = self.inner.with_generation_overrides(overrides);
        self
    }

    fn store_usage(&self, usage: &OpenAiUsage) {
        self.usage
            .record_usage(usage.prompt_tokens, usage.completion_tokens);
        let cached = usage
            .prompt_tokens_details
            .as_ref()
            .map_or(0, |d| d.cached_tokens);
        if cached > 0 {
            self.usage.record_cache(0, cached);
        }
        tracing::debug!(
            prompt_tokens = usage.prompt_tokens,
            cached_tokens = cached,
            completion_tokens = usage.completion_tokens,
            "cocoon API usage"
        );
    }
}

impl LlmProvider for CocoonProvider {
    fn name(&self) -> &'static str {
        "cocoon"
    }

    fn model_identifier(&self) -> &str {
        self.inner.model_identifier()
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_embeddings(&self) -> bool {
        self.embedding_model.is_some()
    }

    fn supports_vision(&self) -> bool {
        false
    }

    fn supports_tool_use(&self) -> bool {
        true
    }

    fn supports_structured_output(&self) -> bool {
        true
    }

    fn last_usage(&self) -> Option<(u64, u64)> {
        self.usage.last_usage()
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        self.usage.last_cache_usage()
    }

    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        self.inner.debug_request_json(messages, tools, stream)
    }

    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        let span = tracing::info_span!("llm.cocoon.request", op = "chat");
        async {
            tracing::debug!(model = self.model_identifier());
            if let Some(ref tx) = self.status_tx {
                let _ = tx.send("Cocoon: sending request...".into());
            }
            let body = self.inner.debug_request_json(messages, &[], false);
            let body_bytes = serde_json::to_vec(&body)
                .map_err(|e| LlmError::Other(format!("body serialization: {e}")))?;

            let response = self
                .client
                .post("/v1/chat/completions", &body_bytes)
                .await?;
            let status = response.status();
            let text = response.text().await.map_err(LlmError::Http)?;

            if !status.is_success() {
                let truncated: String = text.chars().take(256).collect();
                tracing::error!("cocoon API error {status}: {truncated}");
                if status == reqwest::StatusCode::BAD_REQUEST
                    && crate::error::body_is_context_length_error(&text)
                {
                    return Err(LlmError::ContextLengthExceeded);
                }
                return Err(LlmError::ApiError {
                    provider: "cocoon".into(),
                    status: status.as_u16(),
                });
            }

            let resp: OpenAiChatResponse = serde_json::from_str(&text)?;
            if let Some(ref usage) = resp.usage {
                self.store_usage(usage);
            }
            resp.choices
                .into_iter()
                .next()
                .map(|c| c.message.content)
                .ok_or(LlmError::EmptyResponse {
                    provider: "cocoon".into(),
                })
        }
        .instrument(span)
        .await
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        let span = tracing::info_span!("llm.cocoon.request", op = "chat_stream");
        async {
            tracing::debug!(model = self.model_identifier());
            if let Some(ref tx) = self.status_tx {
                let _ = tx.send("Cocoon: streaming...".into());
            }
            let body = self.inner.debug_request_json(messages, &[], true);
            let body_bytes = serde_json::to_vec(&body)
                .map_err(|e| LlmError::Other(format!("body serialization: {e}")))?;

            let response = self
                .client
                .post("/v1/chat/completions", &body_bytes)
                .await?;
            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.map_err(LlmError::Http)?;
                // MINOR-1: tag streaming errors as cocoon-specific for trace distinguishability.
                let truncated: String = text.chars().take(256).collect();
                tracing::error!("cocoon SSE stream error (status={status}): {truncated}");
                if status == reqwest::StatusCode::BAD_REQUEST
                    && crate::error::body_is_context_length_error(&text)
                {
                    return Err(LlmError::ContextLengthExceeded);
                }
                return Err(LlmError::ApiError {
                    provider: "cocoon".into(),
                    status: status.as_u16(),
                });
            }
            Ok(openai_sse_to_stream(response))
        }
        .instrument(span)
        .await
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        let span = tracing::info_span!("llm.cocoon.request", op = "embed");
        async {
            let model = self
                .embedding_model
                .as_deref()
                .ok_or(LlmError::EmbedUnsupported {
                    provider: "cocoon".into(),
                })?;

            let text = truncate_for_embed(text);
            let body = EmbeddingRequest {
                input: &text,
                model,
            };
            let body_bytes = serde_json::to_vec(&body)
                .map_err(|e| LlmError::Other(format!("embed body serialization: {e}")))?;

            let response = self.client.post("/v1/embeddings", &body_bytes).await?;
            let status = response.status();
            let body_text = response.text().await.map_err(LlmError::Http)?;

            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(LlmError::EmbedUnsupported {
                    provider: "cocoon".into(),
                });
            }
            if !status.is_success() {
                let truncated: String = body_text.chars().take(256).collect();
                tracing::error!("cocoon embed error {status}: {truncated}");
                return Err(LlmError::ApiError {
                    provider: "cocoon".into(),
                    status: status.as_u16(),
                });
            }

            let resp: EmbeddingResponse = serde_json::from_str(&body_text)?;
            resp.data
                .into_iter()
                .next()
                .map(|d| d.embedding)
                .ok_or(LlmError::EmptyResponse {
                    provider: "cocoon".into(),
                })
        }
        .instrument(span)
        .await
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, LlmError> {
        let span = tracing::info_span!(
            "llm.cocoon.request",
            op = "embed_batch",
            count = texts.len()
        );
        async {
            if texts.is_empty() {
                return Ok(Vec::new());
            }

            let model = self
                .embedding_model
                .as_deref()
                .ok_or(LlmError::EmbedUnsupported {
                    provider: "cocoon".into(),
                })?;

            let truncated: Vec<std::borrow::Cow<'_, str>> =
                texts.iter().map(|t| truncate_for_embed(t)).collect();
            let refs: Vec<&str> = truncated.iter().map(std::convert::AsRef::as_ref).collect();

            let body = EmbeddingBatchRequest { model, input: refs };
            let body_bytes = serde_json::to_vec(&body)
                .map_err(|e| LlmError::Other(format!("embed_batch body serialization: {e}")))?;

            let response = self.client.post("/v1/embeddings", &body_bytes).await?;
            let status = response.status();
            let body_text = response.text().await.map_err(LlmError::Http)?;

            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(LlmError::EmbedUnsupported {
                    provider: "cocoon".into(),
                });
            }
            if !status.is_success() {
                let truncated_err: String = body_text.chars().take(256).collect();
                tracing::error!("cocoon embed_batch error {status}: {truncated_err}");
                return Err(LlmError::ApiError {
                    provider: "cocoon".into(),
                    status: status.as_u16(),
                });
            }

            let resp: EmbeddingResponse = serde_json::from_str(&body_text)?;
            if resp.data.len() != texts.len() {
                return Err(LlmError::Other(format!(
                    "cocoon returned {} embeddings for {} inputs",
                    resp.data.len(),
                    texts.len()
                )));
            }

            let mut data = resp.data;
            data.sort_unstable_by_key(|d| d.index);
            Ok(data.into_iter().map(|d| d.embedding).collect())
        }
        .instrument(span)
        .await
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        let span = tracing::info_span!("llm.cocoon.request", op = "chat_with_tools");
        async {
            tracing::debug!(model = self.model_identifier());
            let body = serde_json::to_vec(&self.inner.debug_request_json(messages, tools, false))
                .map_err(|e| LlmError::Other(format!("body serialization: {e}")))?;

            let response = self.client.post("/v1/chat/completions", &body).await?;
            let status = response.status();
            let text = response.text().await.map_err(LlmError::Http)?;

            if status == reqwest::StatusCode::BAD_REQUEST {
                let truncated: String = text.chars().take(256).collect();
                tracing::warn!("cocoon tool chat 400 bad request: {truncated}");
                if crate::error::body_is_context_length_error(&text) {
                    return Err(LlmError::ContextLengthExceeded);
                }
                return Err(LlmError::InvalidInput {
                    provider: "cocoon".into(),
                    message: text.chars().take(512).collect(),
                });
            }

            if !status.is_success() {
                let truncated: String = text.chars().take(256).collect();
                tracing::error!("cocoon API error {status}: {truncated}");
                return Err(LlmError::ApiError {
                    provider: "cocoon".into(),
                    status: status.as_u16(),
                });
            }

            let result = self.inner.decode_tool_chat_response(&text, "cocoon")?;
            if let Some((prompt, completion)) = self.inner.last_usage() {
                self.usage.record_usage(prompt, completion);
            }
            if let Some((write, cached)) = self.inner.last_cache_usage() {
                self.usage.record_cache(write, cached);
            }
            Ok(result)
        }
        .instrument(span)
        .await
    }

    async fn chat_typed<T>(&self, messages: &[Message]) -> Result<T, LlmError>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema + 'static,
        Self: Sized,
    {
        let span = tracing::info_span!("llm.cocoon.request", op = "chat_typed");
        async {
            tracing::debug!(model = self.model_identifier());
            let body_bytes = self.inner.build_typed_chat_body::<T>(messages)?;

            let response = self
                .client
                .post("/v1/chat/completions", &body_bytes)
                .await?;
            let status = response.status();
            let text = response.text().await.map_err(LlmError::Http)?;

            if status == reqwest::StatusCode::BAD_REQUEST {
                let truncated: String = text.chars().take(256).collect();
                tracing::warn!("cocoon chat_typed 400 bad request: {truncated}");
                if crate::error::body_is_context_length_error(&text) {
                    return Err(LlmError::ContextLengthExceeded);
                }
                return Err(LlmError::InvalidInput {
                    provider: "cocoon".into(),
                    message: text.chars().take(512).collect(),
                });
            }

            if !status.is_success() {
                let truncated: String = text.chars().take(256).collect();
                tracing::error!("cocoon API error {status}: {truncated}");
                return Err(LlmError::ApiError {
                    provider: "cocoon".into(),
                    status: status.as_u16(),
                });
            }

            let resp: OpenAiChatResponse = serde_json::from_str(&text)?;
            if let Some(ref usage) = resp.usage {
                self.store_usage(usage);
            }

            let content = resp
                .choices
                .into_iter()
                .next()
                .map(|c| c.message.content)
                .ok_or(LlmError::EmptyResponse {
                    provider: "cocoon".into(),
                })?;

            serde_json::from_str::<T>(&content)
                .map_err(|e| LlmError::StructuredParse(e.to_string()))
        }
        .instrument(span)
        .await
    }
}
