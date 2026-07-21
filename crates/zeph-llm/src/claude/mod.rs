// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Claude (Anthropic) LLM provider implementation.
//!
//! [`ClaudeProvider`] wraps the Anthropic Messages API and supports:
//! - Standard chat and streaming via Server-Sent Events
//! - Native tool use (function calling)
//! - Vision (image input in messages)
//! - Extended and adaptive thinking (`claude-sonnet-5`, `claude-opus-4-8`)
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
//! model = "claude-sonnet-5"
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
//! let provider = ClaudeProvider::new("key".into(), "claude-sonnet-5".into(), 16_000)
//!     .with_thinking(ThinkingConfig::Adaptive { effort: Some(ThinkingEffort::High) })?;
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
use crate::tool_desc::build_tool_description;
use crate::usage::UsageTracker;

use crate::provider::{
    ChatResponse, ChatStream, GenerationOverrides, LlmProvider, Message, MessagePart, StatusTx,
    ToolDefinition,
};
use crate::retry::send_with_retry;
use crate::sse::claude_sse_to_stream;

use self::cache::{build_cache_control, log_cache_usage, split_system_into_blocks, tool_cache_key};
use self::request::parse_tool_response;
use self::types::{
    AnthropicContentBlock, AnthropicTool, ApiMessage, ContextManagement, ContextManagementTrigger,
    OutputConfig, RequestBody, StructuredApiMessage, SystemContentBlock, ToolApiResponse,
    ToolChoice, ToolRequestBody, TypedToolRequestBody, VisionRequestBody,
};

use self::types::{budget_to_effort, thinking_capability};
use zeph_config::{CacheTtl, ThinkingConfig, ThinkingEffort};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_BETA_INTERLEAVED_THINKING: &str = "interleaved-thinking-2025-05-14";
const ANTHROPIC_BETA_COMPACT: &str = "compact-2026-01-12";
const ANTHROPIC_BETA_EXTENDED_CONTEXT: &str = "context-1m-2025-08-07";
const ANTHROPIC_BETA_EXTENDED_CACHE_TTL: &str = "extended-cache-ttl-2025-04-25";

/// Models known to support the extended 1-hour cache TTL beta.
const MODELS_WITH_EXTENDED_CACHE_TTL: &[&str] =
    &["claude-opus-4", "claude-sonnet-4", "claude-haiku-4"];
const MAX_RETRIES: u32 = 3;

use self::types::MIN_MAX_TOKENS_WITH_THINKING;
use crate::sse::claude_sse_to_tool_stream;

/// [`LlmProvider`] backend for the Anthropic Claude API.
///
/// Construct with [`ClaudeProvider::new`] and then chain optional builder methods:
/// - [`with_thinking`](Self::with_thinking) — extended or adaptive thinking
/// - [`with_server_compaction`](Self::with_server_compaction) — server-side context compaction
/// - [`with_extended_context`](Self::with_extended_context) — 1M-token context window
/// - [`with_cache_user_messages`](Self::with_cache_user_messages) — prompt caching
/// - [`with_status_tx`](Self::with_status_tx) — real-time status events for the UI
/// - [`with_generation_overrides`](Self::with_generation_overrides) — temperature / top-p
#[allow(clippy::struct_excessive_bools)] // independent boolean flags; bitflags or enum would obscure semantics without reducing complexity
pub struct ClaudeProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    /// The `max_tokens` value captured at construction, before any `with_thinking`/
    /// `set_thinking` call. Immutable baseline used to recompute the effective `max_tokens`
    /// on every runtime thinking change, so enable→disable→re-enable cycles never ratchet
    /// the value upward permanently (see [`Self::set_thinking`]).
    base_max_tokens: u32,
    thinking: Option<ThinkingConfig>,
    pub(crate) status_tx: Option<StatusTx>,
    /// Whether to attach `cache_control` to user messages in multi-turn conversations.
    cache_user_messages: bool,
    usage: UsageTracker,
    /// Cached pre-serialized tool definitions. Keyed by hash of names+schemas; invalidated when the set changes.
    tool_cache: Mutex<Option<(u64, Vec<serde_json::Value>)>>,
    generation_overrides: Option<GenerationOverrides>,
    /// When `true`, append a compact JSON hint of the tool's output schema to its description.
    forward_output_schema: bool,
    /// Maximum bytes of the compact JSON appended as the output schema hint.
    output_schema_hint_bytes: usize,
    /// Maximum bytes of the combined description (base + hint). `usize::MAX` means no cap.
    max_tool_description_bytes: usize,
    /// Enable Claude server-side context compaction (compact-2026-01-12 beta).
    server_compaction: bool,
    /// Set to `true` at runtime when the API rejects the `compact-2026-01-12` beta header
    /// (e.g. header deprecated/removed). Shared via `Arc` so clones observe the same state.
    server_compaction_rejected: Arc<AtomicBool>,
    /// Most recent compaction summary received from the API, if any.
    last_compaction: Mutex<Option<String>>,
    enable_extended_context: bool,
    /// Prompt cache TTL variant. `None` means default (~5 min ephemeral).
    prompt_cache_ttl: Option<CacheTtl>,
    /// SSE buffer size caps (tool JSON, thinking, compaction). Sourced from config.
    stream_limits: zeph_config::StreamLimits,
    /// Name reported by [`LlmProvider::name`]. Defaults to `"claude"`; set the TOML-configured
    /// `name` via [`with_provider_name`](Self::with_provider_name) so that router reputation
    /// tracking and provider selection can distinguish between multiple configured Claude
    /// instances (#5892).
    provider_name: String,
    /// Messages API base URL. Always [`API_URL`] in production; overridable only under
    /// `#[cfg(test)]` via `with_api_url` so tests can point requests at a mock HTTP server.
    api_url: String,
}

impl fmt::Debug for ClaudeProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeProvider")
            .field("client", &"<reqwest::Client>")
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("base_max_tokens", &self.base_max_tokens)
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
            .field("prompt_cache_ttl", &self.prompt_cache_ttl)
            .field("stream_limits", &self.stream_limits)
            .field("forward_output_schema", &self.forward_output_schema)
            .field("output_schema_hint_bytes", &self.output_schema_hint_bytes)
            .field(
                "max_tool_description_bytes",
                &self.max_tool_description_bytes,
            )
            .field("provider_name", &self.provider_name)
            .field("api_url", &self.api_url)
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
            base_max_tokens: self.base_max_tokens,
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
            prompt_cache_ttl: self.prompt_cache_ttl,
            forward_output_schema: self.forward_output_schema,
            output_schema_hint_bytes: self.output_schema_hint_bytes,
            max_tool_description_bytes: self.max_tool_description_bytes,
            stream_limits: self.stream_limits.clone(),
            provider_name: self.provider_name.clone(),
            api_url: self.api_url.clone(),
        }
    }
}

/// Per-family minimum non-stale `(major, minor)` for the modern
/// `claude-<family>-<major>[-<minor>][-<date>]` scheme, plus the current
/// recommended replacement ID. Bump the numbers here (a one-line change) when a
/// family's current release moves up; this never false-positives on a model
/// NEWER than the floor, so it ages better than a hardcoded prefix literal.
const MODERN_MODEL_FLOORS: &[(&str, u32, u32, &str)] = &[
    ("sonnet", 5, 0, "claude-sonnet-5"),
    ("opus", 4, 8, "claude-opus-4-8"),
    ("haiku", 4, 5, "claude-haiku-4-5-20251001"),
    ("fable", 5, 0, "claude-fable-5"),
];

/// Returns the current recommended replacement when `model` is a recognizably
/// stale Claude identifier — either a retired `claude-3*` model or a modern
/// `claude-<family>-<version>` below its family floor. Returns `None` for
/// current, newer-than-floor, or unrecognized model strings (fail-open).
///
/// Limitation: a trailing date suffix on a version-less ID (e.g.
/// `claude-opus-4-20250514`) is misparsed as the minor version, which can
/// mask a stale warning for that exact shape. In-scope IDs use the
/// `major-minor` form and are unaffected.
fn stale_model_suggestion(model: &str) -> Option<&'static str> {
    // Legacy retired generation — preserve #1625 coverage exactly.
    if model.starts_with("claude-3") {
        return Some("claude-sonnet-5 or claude-haiku-4-5-20251001");
    }
    let rest = model.strip_prefix("claude-")?; // non-Claude → fail open
    let mut parts = rest.split('-');
    let family = parts.next()?; // "sonnet", "opus", ...
    let &(_, floor_major, floor_minor, suggestion) =
        MODERN_MODEL_FLOORS.iter().find(|(f, ..)| *f == family)?; // unknown family → fail open
    let major: u32 = parts.next()?.parse().ok()?; // non-numeric (alias) → fail open
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    ((major, minor) < (floor_major, floor_minor)).then_some(suggestion)
}

/// A structured chat history with the no-prefill gate already applied.
///
/// The tuple field is private to this module, so this type is constructible only via the
/// module-private [`Self::new`], called by [`ClaudeProvider::structured_history`] on the
/// production path, after it has stripped trailing assistant turns. `ToolRequestBody`,
/// `VisionRequestBody`, and `TypedToolRequestBody` require `&GatedStructuredHistory` for their
/// `messages` field instead of `&[StructuredApiMessage]` — a request-construction path that
/// calls `request::split_messages_structured` directly and tries to hand its raw `Vec` to a
/// request body now fails to compile with a type mismatch, closing the loophole left open by
/// #6155/#6156 (#6158).
#[derive(serde::Serialize)]
#[serde(transparent)]
pub(in crate::claude) struct GatedStructuredHistory(Vec<StructuredApiMessage>);

impl GatedStructuredHistory {
    fn new(chat: Vec<StructuredApiMessage>) -> Self {
        Self(chat)
    }
}

/// A plain (non-structured) chat history with the no-prefill gate already applied.
///
/// Same construction guarantee as [`GatedStructuredHistory`]: constructible only via the
/// module-private [`Self::new`], called by [`ClaudeProvider::plain_history`] on the production
/// path. `RequestBody`'s `messages` field requires `&GatedPlainHistory` instead of
/// `&[ApiMessage]`.
#[derive(serde::Serialize)]
#[serde(transparent)]
pub(in crate::claude) struct GatedPlainHistory<'m>(Vec<ApiMessage<'m>>);

impl<'m> GatedPlainHistory<'m> {
    fn new(chat: Vec<ApiMessage<'m>>) -> Self {
        Self(chat)
    }
}

impl ClaudeProvider {
    const MAX_CACHE_CONTROL_BLOCKS: usize = 4;

    /// Create a new provider.
    ///
    /// Warns at runtime when `model` is a recognizably stale Claude identifier
    /// (retired `claude-3*`, or below its family's current-version floor) because
    /// those identifiers may be retired or superseded by a newer default.
    #[must_use]
    pub fn new(api_key: String, model: String, max_tokens: u32) -> Self {
        if let Some(suggestion) = stale_model_suggestion(&model) {
            tracing::warn!(
                model = %model,
                "configured Claude model is not a current release and may be retired or \
                superseded; consider upgrading to {suggestion}",
            );
        }
        Self {
            client: crate::http::llm_client(600),
            api_key,
            model,
            max_tokens,
            base_max_tokens: max_tokens,
            thinking: None,
            status_tx: None,
            cache_user_messages: true,
            usage: UsageTracker::default(),
            tool_cache: Mutex::new(None),
            generation_overrides: None,
            forward_output_schema: false,
            output_schema_hint_bytes: 1024,
            max_tool_description_bytes: usize::MAX,
            server_compaction: false,
            server_compaction_rejected: Arc::new(AtomicBool::new(false)),
            last_compaction: Mutex::new(None),
            enable_extended_context: false,
            prompt_cache_ttl: None,
            stream_limits: zeph_config::StreamLimits::default(),
            provider_name: "claude".to_owned(),
            api_url: API_URL.to_owned(),
        }
    }

    /// Override the Messages API base URL. Test-only: points requests at a mock HTTP server
    /// (e.g. `wiremock`) so tests can assert on the wire format Claude actually receives,
    /// instead of only on the request body construction functions in isolation.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_api_url(mut self, url: impl Into<String>) -> Self {
        self.api_url = url.into();
        self
    }

    /// Set the name reported by [`LlmProvider::name`].
    ///
    /// Populate this from the TOML-configured `name` field of the `[[llm.providers]]` entry
    /// so that router reputation tracking and generic embed-provider selection can
    /// distinguish between multiple configured Claude instances. Without this, every
    /// `ClaudeProvider` reports the same literal `"claude"`, which corrupts per-provider
    /// availability tracking and embed routing when more than one Claude entry is configured.
    #[must_use]
    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.provider_name = name.into();
        self
    }

    /// Override generation parameters (temperature, top-p) for this provider.
    #[must_use]
    pub fn with_generation_overrides(mut self, overrides: GenerationOverrides) -> Self {
        self.generation_overrides = Some(overrides);
        self
    }

    /// Enable forwarding of MCP tool output schemas as a description hint.
    ///
    /// When enabled, appends a compact JSON hint of the tool's `output_schema` to its description
    /// (capped at `hint_bytes`). Disabled by default to preserve Anthropic prompt-cache hit rates.
    ///
    /// `max_description_bytes` caps the combined `base + hint` string. Pass `usize::MAX` for no cap.
    #[must_use]
    pub fn with_output_schema_forwarding(
        mut self,
        enabled: bool,
        hint_bytes: usize,
        max_description_bytes: usize,
    ) -> Self {
        self.forward_output_schema = enabled;
        self.output_schema_hint_bytes = hint_bytes;
        self.max_tool_description_bytes = max_description_bytes;
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

    /// Handle a `compact-2026-01-12` beta rejection at a request call site.
    ///
    /// When `*retried` is still `false` and `status`/`text` indicate a beta rejection,
    /// permanently disables server-side compaction for this session, warns once, sets
    /// `*retried = true`, and returns `true` so the caller can `continue` its retry loop.
    /// Returns `false` otherwise, in which case the caller must fall through to normal
    /// error handling. `variant` labels the call site in the warning (e.g. `"streaming"`,
    /// `"typed"`); pass `""` for the non-streaming, non-tool call site.
    fn handle_compact_beta_rejection(
        &self,
        status: reqwest::StatusCode,
        text: &str,
        retried: &mut bool,
        variant: &str,
    ) -> bool {
        if *retried || !Self::is_compact_beta_rejection(status, text) {
            return false;
        }
        self.server_compaction_rejected
            .store(true, Ordering::Relaxed);
        let suffix = if variant.is_empty() {
            String::new()
        } else {
            format!(" ({variant})")
        };
        tracing::warn!(
            "compact-2026-01-12 beta header rejected by Claude API{suffix}; \
            disabling server-side compaction for this session. \
            Update your config to set `server_compaction = false`."
        );
        *retried = true;
        true
    }

    #[must_use]
    pub fn with_extended_context(mut self, enabled: bool) -> Self {
        self.enable_extended_context = enabled;
        if enabled {
            tracing::info!("Claude extended context (1M) enabled");
        }
        self
    }

    /// Set the prompt cache TTL variant for this provider.
    ///
    /// Passing `None` (the default) uses the standard ~5-minute ephemeral TTL at no extra cost.
    /// Passing `Some(CacheTtl::OneHour)` enables the `extended-cache-ttl-2025-04-25` beta and
    /// approximately doubles cache write cost in exchange for far fewer re-writes.
    ///
    /// # Interaction with `with_cache_user_messages`
    ///
    /// The 1-hour TTL is applied to all three cache surfaces: system blocks, the tool list, and
    /// the message-level breakpoint. If [`with_cache_user_messages`](Self::with_cache_user_messages)
    /// was called with `false`, the message-level breakpoint is never placed, so the 1-hour TTL
    /// applies only to system blocks and tools in that configuration.
    #[must_use]
    pub fn with_prompt_cache_ttl(mut self, ttl: Option<CacheTtl>) -> Self {
        if let Some(CacheTtl::OneHour) = ttl {
            let supported = MODELS_WITH_EXTENDED_CACHE_TTL
                .iter()
                .any(|prefix| self.model.starts_with(prefix));
            if !supported {
                tracing::warn!(
                    model = %self.model,
                    "model may not support extended 1h cache TTL beta; \
                    known-supported prefixes: {}",
                    MODELS_WITH_EXTENDED_CACHE_TTL.join(", "),
                );
            }
            tracing::info!(
                model = %self.model,
                "prompt cache TTL set to 1 hour (extended-cache-ttl-2025-04-25 beta); \
                cache writes cost ~2× ephemeral",
            );
        }
        self.prompt_cache_ttl = ttl;
        self
    }

    /// Override SSE streaming buffer caps (tool JSON, thinking, compaction).
    ///
    /// Call this when the config provides non-default `[llm.stream_limits]` values.
    #[must_use]
    pub fn with_stream_limits(mut self, limits: zeph_config::StreamLimits) -> Self {
        self.stream_limits = limits;
        self
    }

    /// Configure thinking mode at runtime, in place.
    ///
    /// `None` restores `max_tokens` to the value captured at construction
    /// (`base_max_tokens`), clearing any thinking-token floor — this is what makes
    /// `/think-tokens off` after `/think-tokens 8k` restore the user's originally configured
    /// `max_tokens` instead of leaving it stuck at the 16k thinking floor.
    ///
    /// `Some(_)` always recomputes the effective `max_tokens` from the immutable
    /// `base_max_tokens` baseline, never from a previously-floored `self.max_tokens` — every
    /// enable call is idempotent regardless of prior state.
    ///
    /// # Errors
    ///
    /// Returns an error if `Some(ThinkingConfig::Extended { budget_tokens })` has
    /// `budget_tokens` outside the API-allowed range `[1024, 128_000]`, or
    /// `budget_tokens >= max_tokens` after the automatic 16 000-token floor is applied.
    pub fn set_thinking(&mut self, thinking: Option<ThinkingConfig>) -> Result<(), LlmError> {
        let Some(thinking) = thinking else {
            self.max_tokens = self.base_max_tokens;
            self.thinking = None;
            return Ok(());
        };

        if let ThinkingConfig::Extended { budget_tokens } = thinking {
            const MIN_BUDGET: u32 = 1_024;
            const MAX_BUDGET: u32 = 128_000;
            if !(MIN_BUDGET..=MAX_BUDGET).contains(&budget_tokens) {
                return Err(LlmError::InvalidInput {
                    provider: "claude".into(),
                    message: format!(
                        "budget_tokens {budget_tokens} is out of range [{MIN_BUDGET}, {MAX_BUDGET}]"
                    ),
                });
            }
            let max_tokens = self.base_max_tokens.max(MIN_MAX_TOKENS_WITH_THINKING);
            if budget_tokens >= max_tokens {
                return Err(LlmError::InvalidInput {
                    provider: "claude".into(),
                    message: format!(
                        "budget_tokens {budget_tokens} must be less than max_tokens {max_tokens}"
                    ),
                });
            }
            self.max_tokens = max_tokens;
        } else {
            self.max_tokens = self.base_max_tokens.max(MIN_MAX_TOKENS_WITH_THINKING);
        }
        self.thinking = Some(thinking);
        Ok(())
    }

    /// Configure thinking mode for Claude extended/adaptive thinking.
    ///
    /// # Errors
    ///
    /// Forwards errors from [`Self::set_thinking`].
    pub fn with_thinking(mut self, thinking: ThinkingConfig) -> Result<Self, LlmError> {
        self.set_thinking(Some(thinking))?;
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

    /// Return the current Extended-thinking token budget, or `None` if thinking is disabled
    /// or set to `Adaptive`.
    #[must_use]
    pub fn current_thinking_budget(&self) -> Option<u32> {
        match self.thinking {
            Some(ThinkingConfig::Extended { budget_tokens }) => Some(budget_tokens),
            _ => None,
        }
    }

    /// Return the current Adaptive-thinking effort level as a lowercase string (`"low"`,
    /// `"medium"`, `"high"`), or `None` if thinking is disabled or set to `Extended`.
    #[must_use]
    pub fn current_reasoning_effort(&self) -> Option<String> {
        match self.thinking {
            Some(ThinkingConfig::Adaptive { effort }) => Some(
                // ThinkingEffort is #[non_exhaustive]; fall back to "medium" for any future
                // variant rather than failing to compile on an upstream addition.
                #[allow(clippy::match_same_arms)]
                match effort.unwrap_or_default() {
                    ThinkingEffort::Low => "low",
                    ThinkingEffort::Medium => "medium",
                    ThinkingEffort::High => "high",
                    _ => "medium",
                }
                .to_owned(),
            ),
            _ => None,
        }
    }

    /// Fetch all available Claude models from the Anthropic API and cache them.
    ///
    /// Paginates until `has_more` is false.
    /// Non-success HTTP responses are returned as [`LlmError::ApiError`] without touching the cache.
    ///
    /// # Errors
    ///
    /// Returns an error if the API request fails or returns an auth error.
    ///
    /// # Panics
    ///
    /// Panics if the hardcoded Anthropic API URL cannot be parsed (impossible in practice).
    #[tracing::instrument(name = "llm.claude.list_models_remote", skip_all)]
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
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                tracing::debug!(status = %status, body = %body, "Claude list_models_remote error body");
                return Err(LlmError::ApiError {
                    provider: "claude".into(),
                    status: status.as_u16(),
                });
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
        cache.save(&models).await?;
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
            Some(ThinkingConfig::Extended { budget_tokens }) if cap.prefers_effort => {
                let effort = budget_to_effort(*budget_tokens);
                tracing::warn!(
                    model = %self.model,
                    budget_tokens,
                    ?effort,
                    "budget_tokens is unsupported for this model; auto-converting to effort"
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
            // Unknown future variants: treat as no thinking.
            _ => (None, None, None),
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

        if self.prompt_cache_ttl.is_some_and(CacheTtl::requires_beta) {
            headers.push(ANTHROPIC_BETA_EXTENDED_CACHE_TTL);
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

    pub(crate) fn get_or_build_api_tools(
        &self,
        tools: &[ToolDefinition],
    ) -> Vec<serde_json::Value> {
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
                let description = build_tool_description(
                    &t.description,
                    t.output_schema.as_ref(),
                    self.forward_output_schema,
                    self.output_schema_hint_bytes,
                    self.max_tool_description_bytes,
                    t.name.as_str(),
                );
                serde_json::json!({
                    "name": t.name,
                    "description": description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        if let Some(Some(obj)) = serialized.last_mut().map(serde_json::Value::as_object_mut) {
            let cc = build_cache_control(self.prompt_cache_ttl);
            obj.insert(
                "cache_control".into(),
                serde_json::to_value(&cc).expect("CacheControl serializes"),
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

    /// Whether the conversation must end on a non-assistant turn for this request.
    ///
    /// Sonnet 4.6+ and the Opus 4.7+/Sonnet 5 generation reject assistant prefill
    /// unconditionally (`rejects_prefill`). Opus 4.6 only rejects it while thinking is
    /// enabled, which is why that case additionally checks `prefers_effort` alongside
    /// `thinking_param`. Either way, the API returns 400 if the message history ends with
    /// an assistant turn. Shared by every request-construction path (`build_request`,
    /// `chat_with_tools`, `chat_with_tools_stream`, `chat_typed`, `debug_request_json`) so the
    /// gate cannot drift out of sync between call sites again (#5903, #6145, #6146).
    fn no_prefill(&self, thinking_param: Option<&types::ThinkingParam>) -> bool {
        let cap = thinking_capability(&self.model);
        cap.rejects_prefill || (cap.prefers_effort && thinking_param.is_some())
    }

    /// Strip trailing assistant messages from a structured chat history when [`Self::no_prefill`]
    /// requires the request to end on a non-assistant turn.
    fn strip_trailing_assistant_structured(
        no_prefill: bool,
        chat_messages: &mut Vec<StructuredApiMessage>,
    ) {
        if !no_prefill {
            return;
        }
        while chat_messages.last().is_some_and(|m| m.role == "assistant") {
            chat_messages.pop();
        }
    }

    /// Strip trailing assistant messages from a plain (non-structured) chat history when
    /// [`Self::no_prefill`] requires the request to end on a non-assistant turn.
    fn strip_trailing_assistant_plain(no_prefill: bool, chat_messages: &mut Vec<ApiMessage<'_>>) {
        if !no_prefill {
            return;
        }
        while chat_messages.last().is_some_and(|m| m.role == "assistant") {
            chat_messages.pop();
        }
    }

    /// Split `messages` into a system prompt and a structured chat history, ready to send:
    /// cache-control blocks are capped at Anthropic's budget and the no-prefill gate has
    /// already been applied.
    ///
    /// This is the production path that builds a [`GatedStructuredHistory`] for a request body
    /// — the wrapper's tuple field is private to this module, so `ToolRequestBody`,
    /// `VisionRequestBody`, and `TypedToolRequestBody` (which all take `&GatedStructuredHistory`
    /// for their `messages` field) cannot be built from a raw `Vec<StructuredApiMessage>`
    /// returned by calling `split_messages_structured` directly. `split_messages_structured`
    /// itself is intentionally NOT imported at this module's top level (see the local `use`
    /// inside this function) to additionally keep it off autocomplete at call sites — belt and
    /// braces on top of the type-level guarantee (#5903, #6146, #6155/#6156, #6158).
    fn structured_history(
        &self,
        messages: &[Message],
        thinking_param: Option<&types::ThinkingParam>,
        cache_tool_blocks: usize,
    ) -> (Option<Vec<SystemContentBlock>>, GatedStructuredHistory) {
        use self::request::split_messages_structured;

        let (system, mut chat_messages) =
            split_messages_structured(messages, self.cache_user_messages, self.prompt_cache_ttl);
        let system_blocks =
            system.map(|s| split_system_into_blocks(&s, &self.model, self.prompt_cache_ttl));
        Self::cap_block_cache_controls(
            cache_tool_blocks,
            system_blocks.as_deref(),
            Some(&mut chat_messages),
        );
        Self::strip_trailing_assistant_structured(
            self.no_prefill(thinking_param),
            &mut chat_messages,
        );
        (system_blocks, GatedStructuredHistory::new(chat_messages))
    }

    /// Split `messages` into a system prompt and a plain chat history, with the no-prefill gate
    /// already applied. Same construction guarantee as [`Self::structured_history`].
    fn plain_history<'m>(
        &self,
        messages: &'m [Message],
        thinking_param: Option<&types::ThinkingParam>,
    ) -> (Option<Vec<SystemContentBlock>>, GatedPlainHistory<'m>) {
        use self::request::split_messages;

        let (system, mut chat_messages) = split_messages(messages);
        Self::strip_trailing_assistant_plain(self.no_prefill(thinking_param), &mut chat_messages);
        let system_blocks =
            system.map(|s| split_system_into_blocks(&s, &self.model, self.prompt_cache_ttl));
        (system_blocks, GatedPlainHistory::new(chat_messages))
    }

    fn build_request(&self, messages: &[Message], stream: bool) -> reqwest::RequestBuilder {
        let (thinking_param, mut temperature, effort) = self.build_thinking_param();
        if thinking_param.is_none()
            && let Some(Some(t)) = self.generation_overrides.as_ref().map(|ov| ov.temperature)
        {
            temperature = Some(t);
        }
        let output_config = effort.map(|e| OutputConfig { effort: e }); // lgtm[rust/cleartext-logging]

        if Self::has_image_parts(messages) {
            let (system_blocks, chat_messages) =
                self.structured_history(messages, thinking_param.as_ref(), 0);
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
                .post(&self.api_url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION);
            if let Some(b) = beta {
                req = req.header("anthropic-beta", b);
            }
            return req.header("content-type", "application/json").json(&body);
        }

        let (system_blocks, chat_messages) = self.plain_history(messages, thinking_param.as_ref());
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
            .post(&self.api_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION);
        if let Some(b) = beta {
            req = req.header("anthropic-beta", b);
        }
        req.header("content-type", "application/json").json(&body)
    }

    #[tracing::instrument(name = "llm.claude.send_request", skip_all)]
    async fn send_request(&self, messages: &[Message]) -> Result<String, LlmError> {
        let mut retried = false;
        loop {
            let response = send_with_retry(
                "Claude",
                MAX_RETRIES,
                self.status_tx.as_ref(),
                Some(&self.usage),
                || self.build_request(messages, false).send(),
            )
            .await?;

            let (status, text) = crate::http::read_response_body(response).await?;

            if !status.is_success() {
                if self.handle_compact_beta_rejection(status, &text, &mut retried, "") {
                    continue;
                }
                tracing::error!("Claude API error {status}: {text}");
                return Err(crate::http::map_error_response(status, &text, "claude"));
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

            return resp
                .content
                .first()
                .map(|c| c.text.clone())
                .ok_or(LlmError::EmptyResponse {
                    provider: "claude".into(),
                });
        }
    }

    /// Send a streaming tool-use request and return a [`crate::sse::ToolSseStream`].
    ///
    /// Used by `SpeculativeStreamDrainer` to intercept `InputJsonDelta` events for early
    /// speculative dispatch while assembling the final `ChatResponse` at stream end.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or the API returns a non-2xx status.
    #[tracing::instrument(
        name = "llm.claude.tools_stream",
        skip_all,
        fields(provider = self.name(), model = self.model_identifier())
    )]
    pub async fn chat_with_tools_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<crate::sse::ToolSseStream, LlmError> {
        let api_tools = self.get_or_build_api_tools(tools);

        let (thinking_param, mut temperature, effort) = self.build_thinking_param();
        if thinking_param.is_none()
            && let Some(Some(t)) = self.generation_overrides.as_ref().map(|ov| ov.temperature)
        {
            temperature = Some(t);
        }
        let output_config = effort.map(|e| OutputConfig { effort: e });
        let (system_blocks, chat_messages) =
            self.structured_history(messages, thinking_param.as_ref(), 1);
        let has_tools = !tools.is_empty();
        let mut body = ToolRequestBody {
            model: &self.model,
            max_tokens: self.max_tokens,
            system: system_blocks,
            messages: &chat_messages,
            tools: &api_tools,
            stream: true,
            thinking: thinking_param,
            output_config,
            temperature,
            context_management: self.context_management(),
        };

        let mut retried = false;
        loop {
            body.context_management = self.context_management();
            let beta = self.beta_header(has_tools);
            let response = send_with_retry(
                "Claude",
                MAX_RETRIES,
                self.status_tx.as_ref(),
                Some(&self.usage),
                || {
                    let mut req = self
                        .client
                        .post(&self.api_url)
                        .header("x-api-key", &self.api_key)
                        .header("anthropic-version", ANTHROPIC_VERSION);
                    if let Some(ref b) = beta {
                        req = req.header("anthropic-beta", b);
                    }
                    req.header("content-type", "application/json")
                        .json(&body)
                        .send()
                },
            )
            .await?;

            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.map_err(LlmError::Http)?;
                if self.handle_compact_beta_rejection(status, &text, &mut retried, "tool stream") {
                    continue;
                }
                tracing::error!("Claude API error {status}: {text}");
                return Err(crate::http::map_error_response(status, &text, "claude"));
            }

            return Ok(claude_sse_to_tool_stream(response, &self.stream_limits));
        }
    }

    #[tracing::instrument(name = "llm.claude.send_stream_request", skip_all)]
    async fn send_stream_request(
        &self,
        messages: &[Message],
    ) -> Result<reqwest::Response, LlmError> {
        let mut retried = false;
        loop {
            let response = send_with_retry(
                "Claude",
                MAX_RETRIES,
                self.status_tx.as_ref(),
                Some(&self.usage),
                || self.build_request(messages, true).send(),
            )
            .await?;

            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.map_err(LlmError::Http)?;
                if self.handle_compact_beta_rejection(status, &text, &mut retried, "streaming") {
                    continue;
                }
                tracing::error!("Claude API streaming request error {status}: {text}");
                return Err(crate::http::map_error_response(status, &text, "claude"));
            }

            return Ok(response);
        }
    }
}

impl LlmProvider for ClaudeProvider {
    fn context_window(&self) -> Option<usize> {
        if self.model.contains("opus")
            || self.model.contains("sonnet")
            || self.model.contains("haiku")
        {
            // Only Opus and Sonnet models support the 1M context window.
            // Haiku does not support extended context even when the flag is set.
            let supports_1m = self.enable_extended_context && !self.model.contains("haiku");
            if supports_1m {
                Some(1_000_000)
            } else {
                if self.enable_extended_context && self.model.contains("haiku") {
                    tracing::warn!(
                        model = %self.model,
                        "enable_extended_context has no effect for Haiku models; \
                        extended context (1M) is only supported by Claude Opus and Sonnet models"
                    );
                }
                Some(200_000)
            }
        } else {
            None
        }
    }

    #[tracing::instrument(
        name = "llm.chat",
        skip_all,
        fields(provider = self.name(), model = self.model_identifier())
    )]
    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        self.send_request(messages).await
    }

    #[tracing::instrument(
        name = "llm.chat_stream",
        skip_all,
        fields(provider = self.name(), model = self.model_identifier())
    )]
    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        let response = self.send_stream_request(messages).await?;
        Ok(claude_sse_to_stream(response, &self.stream_limits))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    #[tracing::instrument(
        name = "llm.embed",
        skip_all,
        fields(provider = self.name(), model = self.model_identifier())
    )]
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
        Err(LlmError::EmbedUnsupported {
            provider: "claude".into(),
        })
    }

    fn supports_embeddings(&self) -> bool {
        false
    }

    fn name(&self) -> &str {
        &self.provider_name
    }

    fn model_identifier(&self) -> &str {
        &self.model
    }

    fn supports_structured_output(&self) -> bool {
        true
    }

    #[allow(clippy::too_many_lines)] // retry loop + body construction are tightly coupled; extracting would obscure control flow
    async fn chat_typed<T>(&self, messages: &[Message]) -> Result<T, LlmError>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema + 'static,
        Self: Sized,
    {
        let (schema_value, _) = crate::provider::cached_schema::<T>()?;
        let type_name = crate::provider::short_type_name::<T>();
        let tool_name = format!("submit_{type_name}");
        let tool = ToolDefinition {
            name: tool_name.clone().into(),
            description: format!("Submit the structured {type_name} result"),
            parameters: schema_value,
            output_schema: None,
        };
        let api_tool = AnthropicTool {
            name: tool.name.as_str(),
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
        let (system_blocks, chat_messages) =
            self.structured_history(messages, thinking_param.as_ref(), 0);
        let tool_choice = ToolChoice {
            r#type: "tool",
            name: &tool_name,
        };
        let mut body = TypedToolRequestBody {
            model: &self.model,
            max_tokens: self.max_tokens,
            system: system_blocks,
            messages: &chat_messages,
            tools: &[api_tool],
            tool_choice,
            thinking: thinking_param,
            output_config,
            temperature,
            context_management: None,
        };
        let mut retried = false;
        loop {
            body.context_management = self.context_management();
            let beta = self.beta_header(true);
            let response = send_with_retry(
                "Claude",
                MAX_RETRIES,
                self.status_tx.as_ref(),
                Some(&self.usage),
                || {
                    let mut req = self
                        .client
                        .post(&self.api_url)
                        .header("x-api-key", &self.api_key)
                        .header("anthropic-version", ANTHROPIC_VERSION);
                    if let Some(ref b) = beta {
                        req = req.header("anthropic-beta", b);
                    }
                    req.header("content-type", "application/json")
                        .json(&body)
                        .send()
                },
            )
            .await?;
            let (status, text) = crate::http::read_response_body(response).await?;
            if !status.is_success() {
                if self.handle_compact_beta_rejection(status, &text, &mut retried, "typed") {
                    continue;
                }
                tracing::error!("Claude API error {status}: {text}");
                return Err(crate::http::map_error_response(status, &text, "claude"));
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
            return Err(LlmError::StructuredParse(
                "no tool_use block in response".into(),
            ));
        }
    }

    fn supports_vision(&self) -> bool {
        true
    }

    fn supports_tool_use(&self) -> bool {
        true
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        self.usage.last_cache_usage()
    }

    fn last_usage(&self) -> Option<(u64, u64)> {
        self.usage.last_usage()
    }

    fn last_ttft_ms(&self) -> Option<u64> {
        self.usage.last_ttft_ms()
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
            let (system_blocks, chat_messages) =
                self.structured_history(messages, thinking_param.as_ref(), 1);
            let api_tools = self.get_or_build_api_tools(tools);
            let body = ToolRequestBody {
                model: &self.model,
                max_tokens: self.max_tokens,
                system: system_blocks,
                messages: &chat_messages,
                tools: &api_tools,
                stream: false,
                thinking: thinking_param,
                output_config,
                temperature,
                context_management: self.context_management(),
            };
            return serde_json::to_value(&body)
                .unwrap_or_else(|e| serde_json::json!({ "serialization_error": e.to_string() }));
        }

        if Self::has_image_parts(messages) {
            let (system_blocks, chat_messages) =
                self.structured_history(messages, thinking_param.as_ref(), 0);
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

        let (system_blocks, chat_messages) = self.plain_history(messages, thinking_param.as_ref());
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

    #[tracing::instrument(
        name = "llm.chat_with_tools",
        skip_all,
        fields(provider = self.name(), model = self.model_identifier(), tool_count = tools.len())
    )]
    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        let api_tools = self.get_or_build_api_tools(tools);

        let (thinking_param, mut temperature, effort) = self.build_thinking_param();
        if thinking_param.is_none()
            && let Some(Some(t)) = self.generation_overrides.as_ref().map(|ov| ov.temperature)
        {
            temperature = Some(t);
        }
        let output_config = effort.map(|e| OutputConfig { effort: e });
        let (system_blocks, chat_messages) =
            self.structured_history(messages, thinking_param.as_ref(), 1);
        let has_tools = !tools.is_empty();
        let mut body = ToolRequestBody {
            model: &self.model,
            max_tokens: self.max_tokens,
            system: system_blocks,
            messages: &chat_messages,
            tools: &api_tools,
            stream: false,
            thinking: thinking_param,
            output_config,
            temperature,
            context_management: self.context_management(),
        };

        let mut retried = false;
        loop {
            body.context_management = self.context_management();
            let beta = self.beta_header(has_tools);
            let response = send_with_retry(
                "Claude",
                MAX_RETRIES,
                self.status_tx.as_ref(),
                Some(&self.usage),
                || {
                    let mut req = self
                        .client
                        .post(&self.api_url)
                        .header("x-api-key", &self.api_key)
                        .header("anthropic-version", ANTHROPIC_VERSION);
                    if let Some(ref b) = beta {
                        req = req.header("anthropic-beta", b);
                    }
                    req.header("content-type", "application/json")
                        .json(&body)
                        .send()
                },
            )
            .await?;

            let (status, text) = crate::http::read_response_body(response).await?;

            if !status.is_success() {
                if self.handle_compact_beta_rejection(status, &text, &mut retried, "tool use") {
                    continue;
                }
                tracing::error!("Claude API error {status}: {text}");
                return Err(crate::http::map_error_response(status, &text, "claude"));
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
            return Ok(parsed);
        }
    }
}
