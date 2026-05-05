// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`GonkaProvider`]: an OpenAI-compatible LLM backend over the Gonka signed transport.
//!
//! All outgoing HTTP requests are signed with a secp256k1 key via [`RequestSigner`] and
//! routed through an [`EndpointPool`] that skips failed nodes automatically.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::embed::truncate_for_embed;
use crate::error::LlmError;
use crate::gonka::endpoints::{EndpointPool, now_ns};
use crate::gonka::signer::RequestSigner;
use crate::openai::OpenAiProvider;
use crate::provider::{ChatResponse, ChatStream, LlmProvider, Message, ToolDefinition};
use crate::sse::openai_sse_to_stream;
use crate::usage::UsageTracker;

// ──────────────────────────────────────────────────────────────────────────────
// Wire types shared with openai (local copies to avoid re-exporting private types)
// ──────────────────────────────────────────────────────────────────────────────

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

// ──────────────────────────────────────────────────────────────────────────────
// GonkaProvider
// ──────────────────────────────────────────────────────────────────────────────

/// LLM provider that routes requests through the Gonka network via ECDSA-signed transport.
///
/// Request bodies are constructed by an inner [`OpenAiProvider`] (the Gonka gateway
/// accepts the same wire format), then signed with a secp256k1 key and sent to the
/// next healthy endpoint from an [`EndpointPool`].
pub struct GonkaProvider {
    /// Used only for body construction and capability flags — never called directly.
    inner: OpenAiProvider,
    signer: Arc<RequestSigner>,
    pool: Arc<EndpointPool>,
    client: reqwest::Client,
    timeout: Duration,
    /// Embedding model name, separate from the chat model stored in `inner`.
    embedding_model: Option<String>,
    usage: UsageTracker,
}

impl std::fmt::Debug for GonkaProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GonkaProvider")
            .field("model", &self.inner.model_identifier())
            .field("embedding_model", &self.embedding_model)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl GonkaProvider {
    /// Construct a new `GonkaProvider`.
    ///
    /// - `model` — chat model name sent in the request body (e.g. `"gpt-4o"`).
    /// - `max_tokens` — upper bound on completion tokens.
    /// - `embedding_model` — if `Some`, enables embedding support via `/embeddings`.
    /// - `timeout` — per-request deadline; wraps every outbound `.await`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use std::time::Duration;
    /// use zeph_llm::gonka::endpoints::{EndpointPool, GonkaEndpoint};
    /// use zeph_llm::gonka::RequestSigner;
    /// use zeph_llm::gonka::GonkaProvider;
    ///
    /// # fn example() -> Result<(), zeph_llm::LlmError> {
    /// let signer = Arc::new(RequestSigner::from_hex(
    ///     "0000000000000000000000000000000000000000000000000000000000000001",
    ///     "gonka",
    /// )?);
    /// let pool = Arc::new(EndpointPool::new(vec![GonkaEndpoint {
    ///     base_url: "https://node1.gonka.ai".into(),
    ///     address: "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6".into(),
    /// }])?);
    /// let provider = GonkaProvider::new(
    ///     signer,
    ///     pool,
    ///     "gpt-4o",
    ///     4096,
    ///     Some("text-embedding-3-small".into()),
    ///     Duration::from_secs(30),
    /// );
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn new(
        signer: Arc<RequestSigner>,
        pool: Arc<EndpointPool>,
        model: impl Into<String>,
        max_tokens: u32,
        embedding_model: Option<String>,
        timeout: Duration,
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
        // HTTP client timeout is generous to avoid double-timeout with tokio::time::timeout.
        let client = crate::http::llm_client(timeout.as_secs().saturating_add(30));
        Self {
            inner,
            signer,
            pool,
            client,
            timeout,
            embedding_model,
            usage: UsageTracker::default(),
        }
    }

    /// Sign `body_bytes` and send a POST to `{endpoint}{path}`, retrying across pool nodes.
    ///
    /// Returns the first successful [`reqwest::Response`] (2xx or 400, which is surfaced
    /// to callers for context-length detection). All other statuses cause the endpoint to
    /// be marked failed and the loop to continue.
    async fn signed_request(
        &self,
        path: &str,
        body_bytes: &[u8],
    ) -> Result<reqwest::Response, LlmError> {
        let max_retries = self.pool.len().min(3);
        let mut last_err = None;
        // Pre-allocate once to avoid a heap allocation on every retry attempt.
        let body_owned: Vec<u8> = body_bytes.to_vec();

        for _ in 0..max_retries {
            let (idx, endpoint) = self.pool.next_indexed();
            let url = format!("{}{path}", endpoint.base_url);

            tracing::debug!(endpoint = %endpoint.base_url, "gonka POST {url}");

            // Fresh timestamp on every attempt — non-replayable signatures.
            let timestamp_ns = u128::from(now_ns());
            // Signing is synchronous and completes before the await boundary.
            let signature = self
                .signer
                .sign(body_bytes, timestamp_ns, &endpoint.address)?;

            let fut = self
                .client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("X-Gonka-Timestamp", timestamp_ns.to_string())
                .header("X-Gonka-Signature", &signature)
                .header("X-Gonka-Sender", self.signer.address())
                .body(body_owned.clone())
                .send();

            match tokio::time::timeout(self.timeout, fut).await {
                Ok(Ok(resp)) if resp.status().is_success() || resp.status().as_u16() == 400 => {
                    tracing::debug!(
                        status = resp.status().as_u16(),
                        endpoint = %endpoint.base_url,
                        "gonka response received"
                    );
                    return Ok(resp);
                }
                Ok(Ok(resp)) => {
                    let status = resp.status().as_u16();
                    tracing::warn!(status, endpoint = %endpoint.base_url, "gonka endpoint error");
                    self.pool.mark_failed(idx, Duration::from_secs(30));
                    last_err = Some(LlmError::ApiError {
                        provider: "gonka".into(),
                        status,
                    });
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, endpoint = %endpoint.base_url, "gonka HTTP error");
                    self.pool.mark_failed(idx, Duration::from_secs(30));
                    last_err = Some(LlmError::Http(e));
                }
                Err(_) => {
                    tracing::warn!(endpoint = %endpoint.base_url, "gonka request timed out");
                    self.pool.mark_failed(idx, Duration::from_mins(1));
                    last_err = Some(LlmError::Timeout);
                }
            }
        }

        Err(last_err.unwrap_or(LlmError::Unavailable))
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
            "gonka API usage"
        );
    }
}

impl LlmProvider for GonkaProvider {
    fn name(&self) -> &'static str {
        "gonka"
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
        tracing::debug!(model = self.model_identifier(), "llm.gonka.chat start");
        let body = self.inner.debug_request_json(messages, &[], false);
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| LlmError::Other(format!("body serialization: {e}")))?;

        let response = self
            .signed_request("/chat/completions", &body_bytes)
            .await?;

        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            tracing::error!("gonka API error {status}: {text}");
            if status == reqwest::StatusCode::BAD_REQUEST
                && crate::error::body_is_context_length_error(&text)
            {
                return Err(LlmError::ContextLengthExceeded);
            }
            return Err(LlmError::ApiError {
                provider: "gonka".into(),
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
                provider: "gonka".into(),
            })
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        tracing::debug!(
            model = self.model_identifier(),
            "llm.gonka.chat_stream start"
        );
        let body = self.inner.debug_request_json(messages, &[], true);
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| LlmError::Other(format!("body serialization: {e}")))?;

        let response = self
            .signed_request("/chat/completions", &body_bytes)
            .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.map_err(LlmError::Http)?;
            tracing::error!("gonka streaming error {status}: {text}");
            if status == reqwest::StatusCode::BAD_REQUEST
                && crate::error::body_is_context_length_error(&text)
            {
                return Err(LlmError::ContextLengthExceeded);
            }
            return Err(LlmError::ApiError {
                provider: "gonka".into(),
                status: status.as_u16(),
            });
        }

        Ok(openai_sse_to_stream(response))
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        tracing::debug!("llm.gonka.embed start");
        let model = self
            .embedding_model
            .as_deref()
            .ok_or(LlmError::EmbedUnsupported {
                provider: "gonka".into(),
            })?;

        let text = truncate_for_embed(text);
        let body = EmbeddingRequest {
            input: &text,
            model,
        };
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| LlmError::Other(format!("embed body serialization: {e}")))?;

        let response = self.signed_request("/embeddings", &body_bytes).await?;

        let status = response.status();
        let body_text = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            tracing::error!("gonka embed error {status}: {body_text}");
            return Err(LlmError::ApiError {
                provider: "gonka".into(),
                status: status.as_u16(),
            });
        }

        let resp: EmbeddingResponse = serde_json::from_str(&body_text)?;
        resp.data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or(LlmError::EmptyResponse {
                provider: "gonka".into(),
            })
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, LlmError> {
        tracing::debug!(count = texts.len(), "llm.gonka.embed_batch start");
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let model = self
            .embedding_model
            .as_deref()
            .ok_or(LlmError::EmbedUnsupported {
                provider: "gonka".into(),
            })?;

        let truncated: Vec<std::borrow::Cow<'_, str>> =
            texts.iter().map(|t| truncate_for_embed(t)).collect();
        let refs: Vec<&str> = truncated.iter().map(std::convert::AsRef::as_ref).collect();

        let body = EmbeddingBatchRequest { model, input: refs };
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| LlmError::Other(format!("embed_batch body serialization: {e}")))?;

        let response = self.signed_request("/embeddings", &body_bytes).await?;

        let status = response.status();
        let body_text = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            tracing::error!("gonka embed_batch error {status}: {body_text}");
            return Err(LlmError::ApiError {
                provider: "gonka".into(),
                status: status.as_u16(),
            });
        }

        let resp: EmbeddingResponse = serde_json::from_str(&body_text)?;

        if resp.data.len() != texts.len() {
            return Err(LlmError::Other(format!(
                "gonka returned {} embeddings for {} inputs",
                resp.data.len(),
                texts.len()
            )));
        }

        let mut data = resp.data;
        data.sort_unstable_by_key(|d| d.index);

        Ok(data.into_iter().map(|d| d.embedding).collect())
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        tracing::debug!(
            model = self.model_identifier(),
            "llm.gonka.chat_with_tools start"
        );
        let body = serde_json::to_vec(&self.inner.debug_request_json(messages, tools, false))
            .map_err(|e| LlmError::Other(format!("body serialization: {e}")))?;

        let response = self.signed_request("/chat/completions", &body).await?;

        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;

        if status == reqwest::StatusCode::BAD_REQUEST {
            let truncated: String = text.chars().take(256).collect();
            tracing::warn!("gonka tool chat 400 bad request: {truncated}");
            if crate::error::body_is_context_length_error(&text) {
                return Err(LlmError::ContextLengthExceeded);
            }
            return Err(LlmError::InvalidInput {
                provider: "gonka".into(),
                message: text.chars().take(512).collect(),
            });
        }

        if !status.is_success() {
            let truncated: String = text.chars().take(256).collect();
            tracing::error!("gonka API error {status}: {truncated}");
            return Err(LlmError::ApiError {
                provider: "gonka".into(),
                status: status.as_u16(),
            });
        }

        let result = self.inner.decode_tool_chat_response(&text, "gonka")?;

        // Sync usage from inner tracker to our own tracker.
        if let Some((prompt, completion)) = self.inner.last_usage() {
            self.usage.record_usage(prompt, completion);
        }
        if let Some((write, cached)) = self.inner.last_cache_usage() {
            self.usage.record_cache(write, cached);
        }

        Ok(result)
    }

    async fn chat_typed<T>(&self, messages: &[Message]) -> Result<T, LlmError>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema + 'static,
        Self: Sized,
    {
        tracing::debug!(
            model = self.model_identifier(),
            "llm.gonka.chat_typed start"
        );
        let body_bytes = self.inner.build_typed_chat_body::<T>(messages)?;

        let response = self
            .signed_request("/chat/completions", &body_bytes)
            .await?;

        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;

        if status == reqwest::StatusCode::BAD_REQUEST {
            let truncated: String = text.chars().take(256).collect();
            tracing::warn!("gonka chat_typed 400 bad request: {truncated}");
            if crate::error::body_is_context_length_error(&text) {
                return Err(LlmError::ContextLengthExceeded);
            }
            return Err(LlmError::InvalidInput {
                provider: "gonka".into(),
                message: text.chars().take(512).collect(),
            });
        }

        if !status.is_success() {
            let truncated: String = text.chars().take(256).collect();
            tracing::error!("gonka API error {status}: {truncated}");
            return Err(LlmError::ApiError {
                provider: "gonka".into(),
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
                provider: "gonka".into(),
            })?;

        serde_json::from_str::<T>(&content).map_err(|e| LlmError::StructuredParse(e.to_string()))
    }
}
