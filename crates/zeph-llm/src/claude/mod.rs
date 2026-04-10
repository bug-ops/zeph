// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Claude (Anthropic) LLM provider implementation.
//!
//! [`ClaudeProvider`] wraps the Anthropic Messages API and supports:
//! - Standard chat and streaming via Server-Sent Events
//! - Native tool use (function calling)
//! - Vision (image input in messages)
//! - Extended and adaptive thinking (`claude-sonnet-4-6`, `claude-opus-4-6`)
//! - Prompt caching (`cache_control` blocks) for cost reduction
//! - Server-side context compaction (compact-2026-01-12 beta)
//! - Extended context window (context-1m-2025-08-07 beta)
//!
//! # Configuration
//!
//! ```toml
//! [[llm.providers]]
//! name = "claude"
//! type = "claude"
//! model = "claude-sonnet-4-6"
//! max_tokens = 8192
//! api_key_vault = "ZEPH_CLAUDE_API_KEY"
//! ```
//!
//! # Extended Thinking
//!
//! Enable via [`ClaudeProvider::with_thinking`]:
//!
//! ```rust,no_run
//! use zeph_llm::claude::ClaudeProvider;
//! use zeph_llm::{ThinkingConfig, ThinkingEffort};
//!
//! # fn build() -> Result<ClaudeProvider, zeph_llm::LlmError> {
//! let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-6".into(), 16_000)
//!     .with_thinking(ThinkingConfig::Extended { budget_tokens: 10_000 })?;
//! # Ok(provider)
//! # }
//! ```

mod cache;
mod request;
#[cfg(test)]
mod tests;
mod types;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use crate::error::LlmError;
use crate::usage::UsageTracker;

use crate::provider::{
    ChatResponse, ChatStream, GenerationOverrides, LlmProvider, Message, MessagePart, StatusTx,
    ToolDefinition,
};
use crate::retry::send_with_retry;
use crate::sse::claude_sse_to_stream;

use self::cache::{log_cache_usage, split_system_into_blocks, tool_cache_key};
use self::request::{parse_tool_response, split_messages, split_messages_structured};
use self::types::{
    AnthropicContentBlock, AnthropicTool, ContextManagement, ContextManagementTrigger,
    OutputConfig, RequestBody, StructuredApiMessage, SystemContentBlock, ToolApiResponse,
    ToolChoice, ToolRequestBody, TypedToolRequestBody, VisionRequestBody,
};

pub use self::types::{ThinkingConfig, ThinkingEffort};
use self::types::{budget_to_effort, thinking_capability};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_BETA_INTERLEAVED_THINKING: &str = "interleaved-thinking-2025-05-14";
const ANTHROPIC_BETA_COMPACT: &str = "compact-2026-01-12";
const ANTHROPIC_BETA_EXTENDED_CONTEXT: &str = "context-1m-2025-08-07";
const MAX_RETRIES: u32 = 3;

use self::types::MIN_MAX_TOKENS_WITH_THINKING;

/// [`LlmProvider`] backend for the Anthropic Claude API.
///
/// Construct with [`ClaudeProvider::new`] and then chain optional builder methods:
/// - [`with_thinking`](Self::with_thinking) — extended or adaptive thinking
/// - [`with_server_compaction`](Self::with_server_compaction) — server-side context compaction
/// - [`with_extended_context`](Self::with_extended_context) — 1M-token context window
/// - [`with_cache_user_messages`](Self::with_cache_user_messages) — prompt caching
/// - [`with_status_tx`](Self::with_status_tx) — real-time status events for the UI
/// - [`with_generation_overrides`](Self::with_generation_overrides) — temperature / top-p
pub struct ClaudeProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    thinking: Option<ThinkingConfig>,
    pub(crate) status_tx: Option<StatusTx>,
    /// Whether to attach `cache_control` to user messages in multi-turn conversations.
    cache_user_messages: bool,
    usage: UsageTracker,
    /// Cached pre-serialized tool definitions. Keyed by hash of names+schemas; invalidated when the set changes.
    tool_cache: Mutex<Option<(u64, Vec<serde_json::Value>)>>,
    generation_overrides: Option<GenerationOverrides>,
    /// Enable Claude server-side context compaction (compact-2026-01-12 beta).
    server_compaction: bool,
    /// Set to `true` at runtime when the API rejects the `compact-2026-01-12` beta header
    /// (e.g. header deprecated/removed). Shared via `Arc` so clones observe the same state.
    server_compaction_rejected: Arc<AtomicBool>,
    /// Most recent compaction summary received from the API, if any.
    last_compaction: Mutex<Option<String>>,
    enable_extended_context: bool,
}

impl fmt::Debug for ClaudeProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeProvider")
            .field("client", &"<reqwest::Client>")
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("thinking", &self.thinking)
            .field("status_tx", &self.status_tx.is_some())
            .field("cache_user_messages", &self.cache_user_messages)
            .field("usage", &self.usage)
            .field(
                "tool_cache",
                &self.tool_cache.lock().as_ref().map(|(hash, _)| *hash),
            )
            .field("generation_overrides", &self.generation_overrides)
            .field("server_compaction", &self.server_compaction)
            .field(
                "server_compaction_rejected",
                &self.server_compaction_rejected.load(Ordering::Relaxed),
            )
            .field(
                "last_compaction",
                &self.last_compaction.lock().as_ref().map(String::len),
            )
            .field("enable_extended_context", &self.enable_extended_context)
            .finish()
    }
}

impl Clone for ClaudeProvider {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            thinking: self.thinking.clone(),
            status_tx: self.status_tx.clone(),
            cache_user_messages: self.cache_user_messages,
            usage: UsageTracker::default(),
            tool_cache: Mutex::new(None),
            generation_overrides: self.generation_overrides.clone(),
            server_compaction: self.server_compaction,
            server_compaction_rejected: Arc::clone(&self.server_compaction_rejected),
            last_compaction: Mutex::new(None),
            enable_extended_context: self.enable_extended_context,
        }
    }
}

impl ClaudeProvider {
    const MAX_CACHE_CONTROL_BLOCKS: usize = 4;

    /// Create a new provider.
    ///
    /// Warns at runtime when `model` starts with `"claude-3"` because those identifiers
    /// refer to retired models that may cause API errors.
    #[must_use]
    pub fn new(api_key: String, model: String, max_tokens: u32) -> Self {
        if model.starts_with("claude-3") {
            tracing::warn!(
                model = %model,
                "configured model is a retired Claude 3 identifier and may cause API errors; \
                consider upgrading to claude-sonnet-4-6 or claude-haiku-4-5-20251001",
            );
        }
        Self {
            client: crate::http::llm_client(600),
            api_key,
            model,
            max_tokens,
            thinking: None,
            status_tx: None,
            cache_user_messages: true,
            usage: UsageTracker::default(),
            tool_cache: Mutex::new(None),
            generation_overrides: None,
            server_compaction: false,
            server_compaction_rejected: Arc::new(AtomicBool::new(false)),
            last_compaction: Mutex::new(None),
            enable_extended_context: false,
        }
    }

    /// Override generation parameters (temperature, top-p) for this provider.
    #[must_use]
    pub fn with_generation_overrides(mut self, overrides: GenerationOverrides) -> Self {
        self.generation_overrides = Some(overrides);
        self
    }

    /// Replace the underlying HTTP client. Mainly used in tests to inject a mock transport.
    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Attach a status event sender so the UI receives retry and fallback notifications.
    #[must_use]
    pub fn with_status_tx(mut self, tx: StatusTx) -> Self {
        self.status_tx = Some(tx);
        self
    }

    /// Control whether `cache_control` breakpoints are added to user messages.
    ///
    /// Enabled by default. Disabling saves a small amount of CPU at the cost of losing
    /// prompt cache hits on repeated system prompts.
    #[must_use]
    pub fn with_cache_user_messages(mut self, enabled: bool) -> Self {
        self.cache_user_messages = enabled;
        self
    }

    /// Enable server-side context compaction (Claude compact-2026-01-12 beta).
    ///
    /// When enabled, the API automatically summarizes long conversations and returns
    /// a `compaction` content block. Client-side compaction should be skipped when
    /// this is active.
    #[must_use]
    pub fn with_server_compaction(mut self, enabled: bool) -> Self {
        if enabled && self.model.contains("haiku") {
            tracing::warn!(
                model = %self.model,
                "server-side compaction (compact-2026-01-12) not supported for Haiku models — \
                disabling"
            );
            self.server_compaction = false;
            return self;
        }
        self.server_compaction = enabled;
        self
    }

    /// Return `true` when server-side compaction is enabled.
    #[must_use]
    pub fn server_compaction_enabled(&self) -> bool {
        self.server_compaction
    }

    /// Return the compaction summary from the most recent API call, if a compaction occurred.
    /// Clears the stored value after reading.
    pub fn take_compaction_summary(&self) -> Option<String> {
        self.last_compaction.lock().take()
    }

    /// Return `true` if the `compact-2026-01-12` beta header was rejected by the API
    /// during a previous request this session.
    #[must_use]
    pub fn is_server_compaction_rejected(&self) -> bool {
        self.server_compaction_rejected.load(Ordering::Relaxed)
    }

    /// Detect whether a 400 response body indicates the `compact-2026-01-12` beta header
    /// was rejected by the API.
    fn is_compact_beta_rejection(status: reqwest::StatusCode, body: &str) -> bool {
        status == reqwest::StatusCode::BAD_REQUEST
            && (body.contains(ANTHROPIC_BETA_COMPACT)
                || body.contains("unknown beta")
                || body.contains("invalid beta")
                || body.contains("context_management"))
    }

    #[must_use]
    pub fn with_extended_context(mut self, enabled: bool) -> Self {
        self.enable_extended_context = enabled;
        if enabled {
            tracing::info!("Claude extended context (1M) enabled");
        }
        self
    }

    /// Configure thinking mode for Claude extended/adaptive thinking.
    ///
    /// # Errors
    ///
    /// Returns an error if `budget_tokens` is outside the API-allowed range
    /// `[1024, 128_000]` or if `budget_tokens >= max_tokens` after the automatic
    /// 16 000-token floor is applied.
    pub fn with_thinking(mut self, thinking: ThinkingConfig) -> Result<Self, LlmError> {
        if let ThinkingConfig::Extended { budget_tokens } = thinking {
            const MIN_BUDGET: u32 = 1_024;
            const MAX_BUDGET: u32 = 128_000;
            if !(MIN_BUDGET..=MAX_BUDGET).contains(&budget_tokens) {
                return Err(LlmError::Other(format!(
                    "budget_tokens {budget_tokens} is out of range [{MIN_BUDGET}, {MAX_BUDGET}]"
                )));
            }
            let max_tokens = self.max_tokens.max(MIN_MAX_TOKENS_WITH_THINKING);
            if budget_tokens >= max_tokens {
                return Err(LlmError::Other(format!(
                    "budget_tokens {budget_tokens} must be less than max_tokens {max_tokens}"
                )));
            }
            self.max_tokens = max_tokens;
        } else {
            self.max_tokens = self.max_tokens.max(MIN_MAX_TOKENS_WITH_THINKING);
        }
        self.thinking = Some(thinking);
        Ok(self)
    }

    /// Configure thinking mode, propagating any validation error.
    ///
    /// # Errors
    ///
    /// Forwards errors from [`Self::with_thinking`].
    pub fn with_thinking_opt(self, thinking: Option<ThinkingConfig>) -> Result<Self, LlmError> {
        match thinking {
            Some(t) => self.with_thinking(t),
            None => Ok(self),
        }
    }

    /// Fetch all available Claude models from the Anthropic API and cache them.
    ///
    /// Paginates until `has_more` is false.
    /// 401/403 responses are returned as `LlmError::Other` without touching the cache.
    ///
    /// # Errors
    ///
    /// Returns an error if the API request fails or returns an auth error.
    ///
    /// # Panics
    ///
    /// Panics if the hardcoded Anthropic API URL cannot be parsed (impossible in practice).
    pub async fn list_models_remote(
        &self,
    ) -> Result<Vec<crate::model_cache::RemoteModelInfo>, LlmError> {
        let mut models: Vec<crate::model_cache::RemoteModelInfo> = Vec::new();
        let mut after_id: Option<String> = None;

        loop {
            // Build URL with cursor as a proper query parameter to avoid injection.
            let url = {
                let mut u = reqwest::Url::parse("https://api.anthropic.com/v1/models")
                    .expect("static URL is valid");
                if let Some(ref cursor) = after_id {
                    u.query_pairs_mut().append_pair("after_id", cursor);
                }
                u
            };

            let resp = self
                .client
                .get(url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .send()
                .await?;

            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(LlmError::Other(format!(
                    "Claude API auth error listing models: {status}"
                )));
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                tracing::debug!(status = %status, body = %body, "Claude list_models_remote error body");
                return Err(LlmError::Other(format!(
                    "Claude list models failed: {status}"
                )));
            }

            let page: serde_json::Value = resp.json().await?;
            if let Some(data) = page.get("data").and_then(|v| v.as_array()) {
                for item in data {
                    let type_field = item
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if type_field != "model" {
                        continue;
                    }
                    let id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let display_name = item
                        .get("display_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&id)
                        .to_string();
                    let created_at = item.get("created_at").and_then(serde_json::Value::as_i64);
                    models.push(crate::model_cache::RemoteModelInfo {
                        id,
                        display_name,
                        context_window: None,
                        created_at,
                    });
                }
            }

            let has_more = page
                .get("has_more")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if !has_more {
                break;
            }
            after_id = page
                .get("last_id")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            if after_id.is_none() {
                break;
            }
        }

        let cache = crate::model_cache::ModelCache::for_slug("claude");
        cache.save(&models)?;
        Ok(models)
    }

    fn build_thinking_param(
        &self,
    ) -> (
        Option<types::ThinkingParam>,
        Option<f64>,
        Option<ThinkingEffort>,
    ) {
        let cap = thinking_capability(&self.model);
        match &self.thinking {
            None => (None, None, None),
            Some(ThinkingConfig::Extended { budget_tokens }) if cap.prefers_effort => {
                let effort = budget_to_effort(*budget_tokens);
                tracing::warn!(
                    model = %self.model,
                    budget_tokens,
                    ?effort,
                    "budget_tokens is deprecated for Opus 4.6; auto-converting to effort"
                );
                (
                    Some(types::ThinkingParam {
                        thinking_type: "adaptive",
                        budget_tokens: None,
                    }),
                    None,
                    Some(effort),
                )
            }
            Some(ThinkingConfig::Extended { budget_tokens }) => (
                Some(types::ThinkingParam {
                    thinking_type: "enabled",
                    budget_tokens: Some(*budget_tokens),
                }),
                None,
                None,
            ),
            Some(ThinkingConfig::Adaptive { effort }) => (
                Some(types::ThinkingParam {
                    thinking_type: "adaptive",
                    budget_tokens: None,
                }),
                None,
                *effort,
            ),
        }
    }

    fn beta_header(&self, has_tools: bool) -> Option<String> {
        let mut headers: Vec<&str> = Vec::new();

        if self.enable_extended_context {
            headers.push(ANTHROPIC_BETA_EXTENDED_CONTEXT);
        }

        let cap = thinking_capability(&self.model);
        if self.thinking.is_some()
            && has_tools
            && cap.needs_interleaved_beta
            && matches!(self.thinking, Some(ThinkingConfig::Extended { .. }))
        {
            headers.push(ANTHROPIC_BETA_INTERLEAVED_THINKING);
        }

        if self.server_compaction && !self.server_compaction_rejected.load(Ordering::Relaxed) {
            headers.push(ANTHROPIC_BETA_COMPACT);
        }

        if headers.is_empty() {
            None
        } else {
            Some(headers.join(","))
        }
    }

    /// Build the `context_management` field for server-side compaction.
    /// Returns `None` when `server_compaction` is disabled or the beta header was rejected.
    fn context_management(&self) -> Option<ContextManagement> {
        if !self.server_compaction || self.server_compaction_rejected.load(Ordering::Relaxed) {
            return None;
        }
        let context_window =
            u32::try_from(self.context_window().unwrap_or(200_000)).unwrap_or(200_000_u32);
        // Default hard_compaction_threshold of 0.90 — matches client-side default.
        // Multiply before dividing to preserve precision (avoid losing up to 99 tokens).
        let trigger_tokens = context_window * 80 / 100;
        Some(ContextManagement {
            trigger: ContextManagementTrigger {
                kind: "input_tokens",
                value: trigger_tokens,
            },
            pause_after_compaction: false,
        })
    }

    fn get_or_build_api_tools(&self, tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
        let key = tool_cache_key(tools);
        let mut guard = self.tool_cache.lock();
        if let Some((cached_key, ref cached_values)) = *guard
            && cached_key == key
        {
            return cached_values.clone();
        }
        let mut serialized: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        if let Some(Some(obj)) = serialized.last_mut().map(serde_json::Value::as_object_mut) {
            obj.insert(
                "cache_control".into(),
                serde_json::json!({"type": "ephemeral"}),
            );
        }
        *guard = Some((key, serialized.clone()));
        serialized
    }

    fn store_cache_usage(&self, usage: &types::ApiUsage) {
        self.usage.record_cache(
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
        );
        self.usage
            .record_usage(usage.input_tokens, usage.output_tokens);
    }

    fn has_image_parts(messages: &[Message]) -> bool {
        messages
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, MessagePart::Image(_))))
    }

    fn cap_block_cache_controls(
        tool_blocks: usize,
        system_blocks: Option<&[SystemContentBlock]>,
        chat_messages: Option<&mut Vec<StructuredApiMessage>>,
    ) {
        let tagged_blocks = tool_blocks
            + system_blocks.map_or(0, |system| {
                system
                    .iter()
                    .filter(|block| block.cache_control.is_some())
                    .count()
            });

        if tagged_blocks >= Self::MAX_CACHE_CONTROL_BLOCKS {
            Self::clear_message_cache_controls(chat_messages);
            return;
        }

        let remaining = Self::MAX_CACHE_CONTROL_BLOCKS - tagged_blocks;
        Self::retain_last_message_cache_controls(chat_messages, remaining);
    }

    fn clear_message_cache_controls(chat_messages: Option<&mut Vec<StructuredApiMessage>>) {
        Self::retain_last_message_cache_controls(chat_messages, 0);
    }

    fn retain_last_message_cache_controls(
        chat_messages: Option<&mut Vec<StructuredApiMessage>>,
        keep: usize,
    ) {
        let mut seen = 0usize;
        if let Some(chat) = chat_messages {
            for message in chat.iter_mut().rev() {
                let types::StructuredContent::Blocks(blocks) = &mut message.content else {
                    continue;
                };
                for block in blocks.iter_mut().rev() {
                    let maybe_cache = match block {
                        AnthropicContentBlock::Text { cache_control, .. }
                        | AnthropicContentBlock::ToolResult { cache_control, .. } => {
                            Some(cache_control)
                        }
                        AnthropicContentBlock::ToolUse { .. }
                        | AnthropicContentBlock::Image { .. }
                        | AnthropicContentBlock::Thinking { .. }
                        | AnthropicContentBlock::RedactedThinking { .. }
                        | AnthropicContentBlock::Compaction { .. } => None,
                    };
                    if let Some(cache_control) = maybe_cache
                        && cache_control.is_some()
                    {
                        if seen < keep {
                            seen += 1;
                        } else {
                            *cache_control = None;
                        }
                    }
                }
            }
        }
    }

    fn build_request(&self, messages: &[Message], stream: bool) -> reqwest::RequestBuilder {
        let (thinking_param, mut temperature, effort) = self.build_thinking_param();
        if thinking_param.is_none()
            && let Some(Some(t)) = self.generation_overrides.as_ref().map(|ov| ov.temperature)
        {
            temperature = Some(t);
        }
        let output_config = effort.map(|e| OutputConfig { effort: e }); // lgtm[rust/cleartext-logging]

        let cap = thinking_capability(&self.model);
        // Opus 4.6 with thinking enabled does not support prefill: strip trailing assistant
        // messages so the conversation always ends with a user turn.
        let no_prefill = cap.prefers_effort && thinking_param.is_some();

        if Self::has_image_parts(messages) {
            let (system, mut chat_messages) =
                split_messages_structured(messages, self.cache_user_messages);
            let system_blocks = system.map(|s| split_system_into_blocks(&s, &self.model));
            Self::cap_block_cache_controls(0, system_blocks.as_deref(), Some(&mut chat_messages));
            if no_prefill {
                while chat_messages.last().is_some_and(|m| m.role == "assistant") {
                    chat_messages.pop();
                }
            }
            let beta = self.beta_header(false);
            let body = VisionRequestBody {
                model: &self.model,
                max_tokens: self.max_tokens,
                system: system_blocks,
                messages: &chat_messages,
                stream,
                thinking: thinking_param,
                output_config,
                temperature,
                context_management: self.context_management(),
            };
            let mut req = self
                .client
                .post(API_URL)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION);
            if let Some(b) = beta {
                req = req.header("anthropic-beta", b);
            }
            return req.header("content-type", "application/json").json(&body);
        }

        let (system, mut chat_messages) = split_messages(messages);
        if no_prefill {
            while chat_messages.last().is_some_and(|m| m.role == "assistant") {
                chat_messages.pop();
            }
        }
        let system_blocks = system.map(|s| split_system_into_blocks(&s, &self.model));
        let beta = self.beta_header(false);
        let body = RequestBody {
            model: &self.model,
            max_tokens: self.max_tokens,
            system: system_blocks,
            messages: &chat_messages,
            stream,
            thinking: thinking_param,
            output_config,
            temperature,
            context_management: self.context_management(),
        };

        let mut req = self
            .client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION);
        if let Some(b) = beta {
            req = req.header("anthropic-beta", b);
        }
        req.header("content-type", "application/json").json(&body)
    }

    async fn send_request(&self, messages: &[Message]) -> Result<String, LlmError> {
        let response = send_with_retry("Claude", MAX_RETRIES, self.status_tx.as_ref(), || {
            self.build_request(messages, false).send()
        })
        .await?;

        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            if Self::is_compact_beta_rejection(status, &text) {
                self.server_compaction_rejected
                    .store(true, Ordering::Relaxed);
                tracing::warn!(
                    "compact-2026-01-12 beta header rejected by Claude API; \
                    disabling server-side compaction for this session. \
                    Update your config to set `server_compaction = false`."
                );
                return Err(LlmError::BetaHeaderRejected {
                    header: ANTHROPIC_BETA_COMPACT.into(),
                });
            }
            tracing::error!("Claude API error {status}: {text}");
            return Err(LlmError::Other(format!(
                "Claude API request failed (status {status})"
            )));
        }

        if Self::has_image_parts(messages) {
            let resp: ToolApiResponse = serde_json::from_str(&text)?;
            if let Some(ref usage) = resp.usage {
                log_cache_usage(usage);
                self.store_cache_usage(usage);
            }
            let extracted = resp.content.into_iter().find_map(|b| {
                if let AnthropicContentBlock::Text { text, .. } = b {
                    Some(text)
                } else {
                    None
                }
            });
            return extracted.ok_or(LlmError::EmptyResponse {
                provider: "claude".into(),
            });
        }

        let resp: types::ApiResponse = serde_json::from_str(&text)?;

        if let Some(ref usage) = resp.usage {
            log_cache_usage(usage);
            self.store_cache_usage(usage);
        }

        resp.content
            .first()
            .map(|c| c.text.clone())
            .ok_or(LlmError::EmptyResponse {
                provider: "claude".into(),
            })
    }

    async fn send_stream_request(
        &self,
        messages: &[Message],
    ) -> Result<reqwest::Response, LlmError> {
        let response = send_with_retry("Claude", MAX_RETRIES, self.status_tx.as_ref(), || {
            self.build_request(messages, true).send()
        })
        .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.map_err(LlmError::Http)?;
            if Self::is_compact_beta_rejection(status, &text) {
                self.server_compaction_rejected
                    .store(true, Ordering::Relaxed);
                tracing::warn!(
                    "compact-2026-01-12 beta header rejected by Claude API (streaming); \
                    disabling server-side compaction for this session. \
                    Update your config to set `server_compaction = false`."
                );
                return Err(LlmError::BetaHeaderRejected {
                    header: ANTHROPIC_BETA_COMPACT.into(),
                });
            }
            tracing::error!("Claude API streaming request error {status}: {text}");
            return Err(LlmError::Other(format!(
                "Claude API streaming request failed (status {status})"
            )));
        }

        Ok(response)
    }
}

impl LlmProvider for ClaudeProvider {
    fn context_window(&self) -> Option<usize> {
        if self.model.contains("opus")
            || self.model.contains("sonnet")
            || self.model.contains("haiku")
        {
            // Only Opus 4.6 and Sonnet 4.6 support the 1M context window.
            // Haiku does not support extended context even when the flag is set.
            let supports_1m = self.enable_extended_context && !self.model.contains("haiku");
            if supports_1m {
                Some(1_000_000)
            } else {
                if self.enable_extended_context && self.model.contains("haiku") {
                    tracing::warn!(
                        model = %self.model,
                        "enable_extended_context has no effect for Haiku models; \
                        extended context (1M) is only supported by Opus 4.6 and Sonnet 4.6"
                    );
                }
                Some(200_000)
            }
        } else {
            None
        }
    }

    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        self.send_request(messages).await
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        let response = self.send_stream_request(messages).await?;
        Ok(claude_sse_to_stream(response))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
        Err(LlmError::EmbedUnsupported {
            provider: "claude".into(),
        })
    }

    fn supports_embeddings(&self) -> bool {
        false
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "claude"
    }

    fn model_identifier(&self) -> &str {
        &self.model
    }

    fn supports_structured_output(&self) -> bool {
        true
    }

    async fn chat_typed<T>(&self, messages: &[Message]) -> Result<T, LlmError>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema + 'static,
        Self: Sized,
    {
        let (schema_value, _) = crate::provider::cached_schema::<T>()?;
        let type_name = crate::provider::short_type_name::<T>();

        let tool_name = format!("submit_{type_name}");
        let tool = ToolDefinition {
            name: tool_name.clone(),
            description: format!("Submit the structured {type_name} result"),
            parameters: schema_value,
        };

        let (system, mut chat_messages) =
            split_messages_structured(messages, self.cache_user_messages);
        let api_tool = AnthropicTool {
            name: &tool.name,
            description: &tool.description,
            input_schema: &tool.parameters,
        };

        let (thinking_param, mut temperature, effort) = self.build_thinking_param();
        if thinking_param.is_none()
            && let Some(Some(t)) = self.generation_overrides.as_ref().map(|ov| ov.temperature)
        {
            temperature = Some(t);
        }
        let output_config = effort.map(|e| OutputConfig { effort: e });
        let system_blocks = system.map(|s| split_system_into_blocks(&s, &self.model));
        Self::cap_block_cache_controls(0, system_blocks.as_deref(), Some(&mut chat_messages));
        let beta = self.beta_header(true);
        let body = TypedToolRequestBody {
            model: &self.model,
            max_tokens: self.max_tokens,
            system: system_blocks,
            messages: &chat_messages,
            tools: &[api_tool],
            tool_choice: ToolChoice {
                r#type: "tool",
                name: &tool_name,
            },
            thinking: thinking_param,
            output_config,
            temperature,
            context_management: self.context_management(),
        };

        let mut req = self
            .client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION);
        if let Some(b) = beta {
            req = req.header("anthropic-beta", b);
        }
        let response = req
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            if Self::is_compact_beta_rejection(status, &text) {
                self.server_compaction_rejected
                    .store(true, Ordering::Relaxed);
                tracing::warn!(
                    "compact-2026-01-12 beta header rejected by Claude API (typed); \
                    disabling server-side compaction for this session. \
                    Update your config to set `server_compaction = false`."
                );
                return Err(LlmError::BetaHeaderRejected {
                    header: ANTHROPIC_BETA_COMPACT.into(),
                });
            }
            return Err(LlmError::Other(format!(
                "Claude API request failed (status {status})"
            )));
        }

        let resp: ToolApiResponse = serde_json::from_str(&text)?;

        if let Some(ref usage) = resp.usage {
            log_cache_usage(usage);
            self.store_cache_usage(usage);
        }

        for block in resp.content {
            if let AnthropicContentBlock::ToolUse { input, .. } = block {
                return serde_json::from_value::<T>(input)
                    .map_err(|e| LlmError::StructuredParse(e.to_string()));
            }
        }

        Err(LlmError::StructuredParse(
            "no tool_use block in response".into(),
        ))
    }

    fn supports_vision(&self) -> bool {
        true
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        self.usage.last_cache_usage()
    }

    fn last_usage(&self) -> Option<(u64, u64)> {
        self.usage.last_usage()
    }

    fn take_compaction_summary(&self) -> Option<String> {
        ClaudeProvider::take_compaction_summary(self)
    }

    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        let (thinking_param, mut temperature, effort) = self.build_thinking_param();
        if thinking_param.is_none()
            && let Some(Some(t)) = self.generation_overrides.as_ref().map(|ov| ov.temperature)
        {
            temperature = Some(t);
        }
        let output_config = effort.map(|e| OutputConfig { effort: e });

        if !tools.is_empty() {
            let (system, mut chat_messages) =
                split_messages_structured(messages, self.cache_user_messages);
            let system_blocks = system.map(|s| split_system_into_blocks(&s, &self.model));
            Self::cap_block_cache_controls(1, system_blocks.as_deref(), Some(&mut chat_messages));
            let api_tools = self.get_or_build_api_tools(tools);
            let body = ToolRequestBody {
                model: &self.model,
                max_tokens: self.max_tokens,
                system: system_blocks,
                messages: &chat_messages,
                tools: &api_tools,
                thinking: thinking_param,
                output_config,
                temperature,
                context_management: self.context_management(),
            };
            return serde_json::to_value(&body)
                .unwrap_or_else(|e| serde_json::json!({ "serialization_error": e.to_string() }));
        }

        if Self::has_image_parts(messages) {
            let (system, mut chat_messages) =
                split_messages_structured(messages, self.cache_user_messages);
            let system_blocks = system.map(|s| split_system_into_blocks(&s, &self.model));
            Self::cap_block_cache_controls(0, system_blocks.as_deref(), Some(&mut chat_messages));
            let body = VisionRequestBody {
                model: &self.model,
                max_tokens: self.max_tokens,
                system: system_blocks,
                messages: &chat_messages,
                stream,
                thinking: thinking_param,
                output_config,
                temperature,
                context_management: self.context_management(),
            };
            return serde_json::to_value(&body)
                .unwrap_or_else(|e| serde_json::json!({ "serialization_error": e.to_string() }));
        }

        let (system, chat_messages) = split_messages(messages);
        let system_blocks = system.map(|s| split_system_into_blocks(&s, &self.model));
        let body = RequestBody {
            model: &self.model,
            max_tokens: self.max_tokens,
            system: system_blocks,
            messages: &chat_messages,
            stream,
            thinking: thinking_param,
            output_config,
            temperature,
            context_management: self.context_management(),
        };
        serde_json::to_value(&body)
            .unwrap_or_else(|e| serde_json::json!({ "serialization_error": e.to_string() }))
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        let (system, mut chat_messages) =
            split_messages_structured(messages, self.cache_user_messages);
        let api_tools = self.get_or_build_api_tools(tools);

        let (thinking_param, mut temperature, effort) = self.build_thinking_param();
        if thinking_param.is_none()
            && let Some(Some(t)) = self.generation_overrides.as_ref().map(|ov| ov.temperature)
        {
            temperature = Some(t);
        }
        let output_config = effort.map(|e| OutputConfig { effort: e });
        let system_blocks = system.map(|s| split_system_into_blocks(&s, &self.model));
        Self::cap_block_cache_controls(1, system_blocks.as_deref(), Some(&mut chat_messages));
        let beta = self.beta_header(!tools.is_empty());
        let body = ToolRequestBody {
            model: &self.model,
            max_tokens: self.max_tokens,
            system: system_blocks,
            messages: &chat_messages,
            tools: &api_tools,
            thinking: thinking_param,
            output_config,
            temperature,
            context_management: self.context_management(),
        };

        let response = send_with_retry("Claude", MAX_RETRIES, self.status_tx.as_ref(), || {
            let mut req = self
                .client
                .post(API_URL)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION);
            if let Some(ref b) = beta {
                req = req.header("anthropic-beta", b);
            }
            req.header("content-type", "application/json")
                .json(&body)
                .send()
        })
        .await?;

        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            if Self::is_compact_beta_rejection(status, &text) {
                self.server_compaction_rejected
                    .store(true, Ordering::Relaxed);
                tracing::warn!(
                    "compact-2026-01-12 beta header rejected by Claude API (tool use); \
                    disabling server-side compaction for this session. \
                    Update your config to set `server_compaction = false`."
                );
                return Err(LlmError::BetaHeaderRejected {
                    header: ANTHROPIC_BETA_COMPACT.into(),
                });
            }
            tracing::error!("Claude API error {status}: {text}");
            return Err(LlmError::Other(format!(
                "Claude API request failed (status {status})"
            )));
        }

        let resp: ToolApiResponse = serde_json::from_str(&text)?;
        tracing::debug!(
            stop_reason = ?resp.stop_reason,
            content_blocks = resp.content.len(),
            "Claude chat_with_tools response"
        );
        if let Some(ref usage) = resp.usage {
            log_cache_usage(usage);
            self.store_cache_usage(usage);
        }
        let (parsed, compaction_summary) = parse_tool_response(resp);
        if let Some(ref summary) = compaction_summary {
            tracing::info!(
                summary_len = summary.len(),
                "storing server compaction summary"
            );
            *self.last_compaction.lock() = compaction_summary;
        }
        tracing::debug!(?parsed, "parsed ChatResponse");
        Ok(parsed)
    }
}
