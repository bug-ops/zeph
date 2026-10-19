// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::future::Future;
use std::pin::Pin;
use std::{
    any::TypeId,
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

use futures_core::Stream;
use serde::{Deserialize, Serialize};

use zeph_common::ToolName;

pub use zeph_common::ToolDefinition;

use crate::embed::owned_strs;
use crate::error::LlmError;
use crate::thinking_replay::ThinkingReplay;

static SCHEMA_CACHE: LazyLock<Mutex<HashMap<TypeId, (serde_json::Value, String)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Return the JSON schema value and pretty-printed string for type `T`, cached by `TypeId`.
///
/// # Errors
///
/// Returns an error if schema serialization fails.
pub(crate) fn cached_schema<T: schemars::JsonSchema + 'static>()
-> Result<(serde_json::Value, String), crate::LlmError> {
    let type_id = TypeId::of::<T>();
    if let Ok(cache) = SCHEMA_CACHE.lock()
        && let Some(entry) = cache.get(&type_id)
    {
        return Ok(entry.clone());
    }
    let schema = schemars::schema_for!(T);
    let value = serde_json::to_value(&schema)
        .map_err(|e| crate::LlmError::StructuredParse(e.to_string()))?;
    let pretty = serde_json::to_string_pretty(&schema)
        .map_err(|e| crate::LlmError::StructuredParse(e.to_string()))?;
    if let Ok(mut cache) = SCHEMA_CACHE.lock() {
        cache.insert(type_id, (value.clone(), pretty.clone()));
    }
    Ok((value, pretty))
}

/// Extract the short (unqualified) type name for schema prompts and tool names.
///
/// Returns the last `::` segment of [`std::any::type_name::<T>()`], which is always
/// non-empty. The `"Output"` fallback is unreachable in practice (`type_name` never returns
/// an empty string and `rsplit` on a non-empty string always yields at least one element),
/// but is kept for defensive clarity.
///
/// # Examples
///
/// ```
/// struct MyOutput;
/// // short_type_name::<MyOutput>() returns "MyOutput"
/// ```
pub(crate) fn short_type_name<T: ?Sized>() -> &'static str {
    std::any::type_name::<T>()
        .rsplit("::")
        .next()
        .unwrap_or("Output")
}

/// Per-call extras returned alongside the chat response by [`LlmProvider::chat_with_extras`].
///
/// Always paired 1:1 with a single response — no shared state, no races possible.
/// All optional fields default to `None` so providers that do not expose the
/// underlying API (e.g. Claude, Gemini) can simply return the default.
///
/// Marked `#[non_exhaustive]` so future fields (e.g. `cached_tokens`) can be added
/// without breaking match sites.
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct ChatExtras {
    /// Mean negative log-probability of the generated tokens, when the provider
    /// was configured to request `logprobs` and the API supplied them.
    ///
    /// Lower = more confident. Typical range: `[0.0, ~6.0]` for natural-language tokens.
    pub entropy: Option<f64>,
}

impl ChatExtras {
    /// Return a `ChatExtras` with the given entropy value.
    ///
    /// Used by `MockProvider` (test-only, enabled via `testing` feature) and OpenAI/Ollama providers.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::provider::ChatExtras;
    ///
    /// let extras = ChatExtras::with_entropy(0.9);
    /// assert_eq!(extras.entropy, Some(0.9));
    /// ```
    #[must_use]
    pub fn with_entropy(entropy: f64) -> Self {
        Self {
            entropy: Some(entropy),
        }
    }
}

/// A chunk from an LLM streaming response.
///
/// Consumers should match all variants: future providers may emit non-`Content` chunks
/// that callers must not silently drop (e.g. thinking blocks that must be echoed back).
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// Regular response text.
    Content(String),
    /// Internal reasoning/thinking token (e.g. Claude extended thinking, `OpenAI` reasoning).
    Thinking(String),
    /// Server-side compaction summary (Claude compact-2026-01-12 beta).
    /// Delivered when the Claude API automatically summarizes conversation history.
    Compaction(String),
    /// One or more tool calls from the model received during streaming.
    ToolUse(Vec<ToolUseRequest>),
}

/// Boxed stream of typed chunks from an LLM provider.
///
/// Obtain via [`LlmProvider::chat_stream`]. Drive the stream with
/// `futures::StreamExt::next` or `tokio_stream::StreamExt::next`.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, LlmError>> + Send>>;

/// Structured tool invocation request from the model.
///
/// Returned by [`LlmProvider::chat_with_tools`] when the model decides to call one or
/// more tools. The caller is responsible for executing the tool and returning results
/// via a [`MessagePart::ToolResult`] in the next turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseRequest {
    /// Opaque call identifier assigned by the model; must be echoed in `ToolResult.tool_use_id`.
    pub id: String,
    /// Name of the tool to invoke, matching a [`ToolDefinition::name`].
    pub name: ToolName,
    /// JSON arguments the model wants to pass to the tool.
    pub input: serde_json::Value,
}

/// Token usage billed outside the top-level request counters, such as server-side compaction
/// iterations. Counts toward cost, never toward context size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuxiliaryUsage {
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache-creation input tokens.
    pub cache_creation: u64,
    /// Cache-read input tokens.
    pub cache_read: u64,
}

impl AuxiliaryUsage {
    /// Add `other` to `self`, saturating on overflow.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::provider::AuxiliaryUsage;
    ///
    /// let mut a = AuxiliaryUsage { input: 1, output: 2, cache_creation: 3, cache_read: 4 };
    /// a.add(AuxiliaryUsage { input: 1, ..AuxiliaryUsage::default() });
    /// assert_eq!(a.input, 2);
    /// ```
    pub fn add(&mut self, other: Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_creation = self.cache_creation.saturating_add(other.cache_creation);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
    }
}

/// Thinking block returned by Claude when extended or adaptive thinking is enabled.
///
/// Both variants must be echoed verbatim in the next turn's `assistant` message so
/// the API can correctly attribute reasoning across turns. Never modify or discard
/// these blocks between turns.
#[non_exhaustive]
#[derive(Clone)]
pub enum ThinkingBlock {
    /// Visible reasoning token with its cryptographic signature.
    Thinking { thinking: String, signature: String },
    /// Redacted reasoning block (API-side privacy redaction). Preserved as opaque data.
    Redacted { data: String },
}

impl std::fmt::Debug for ThinkingBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Thinking {
                thinking,
                signature,
            } => f
                .debug_struct("Thinking")
                .field("thinking_len", &thinking.len())
                .field("signature_len", &signature.len())
                .finish(),
            Self::Redacted { data } => f
                .debug_struct("Redacted")
                .field("data_len", &data.len())
                .finish(),
        }
    }
}

/// Marker injected into `ChatResponse::Text` when the LLM response was cut off by the
/// token limit. Consumers can detect this substring to signal `MaxTokens` stop reason.
pub const MAX_TOKENS_TRUNCATION_MARKER: &str = "max_tokens limit reached";

/// One content block of an assistant response, in the order the model produced it.
///
/// Thinking blocks must be echoed verbatim and in position in the next request, so the
/// response keeps the full block sequence rather than splitting it into per-kind lists.
#[derive(Debug, Clone)]
pub enum AssistantBlock {
    /// Extended or adaptive reasoning block (plain or redacted).
    Thinking(ThinkingBlock),
    /// Assistant text.
    Text(String),
    /// Tool invocation request.
    ToolUse(ToolUseRequest),
}

impl AssistantBlock {
    fn as_tool_use(&self) -> Option<&ToolUseRequest> {
        match self {
            Self::ToolUse(call) => Some(call),
            _ => None,
        }
    }

    fn into_tool_use(self) -> Option<ToolUseRequest> {
        match self {
            Self::ToolUse(call) => Some(call),
            _ => None,
        }
    }
}

impl From<&ThinkingBlock> for MessagePart {
    fn from(block: &ThinkingBlock) -> Self {
        match block {
            ThinkingBlock::Thinking {
                thinking,
                signature,
            } => Self::ThinkingBlock {
                thinking: thinking.clone(),
                signature: signature.clone(),
            },
            ThinkingBlock::Redacted { data } => Self::RedactedThinkingBlock { data: data.clone() },
        }
    }
}

/// Ordered content of an assistant tool-use response.
///
/// The block sequence is the single source of truth; [`text`](Self::text) and
/// [`tool_calls`](Self::tool_calls) are derived views.
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{AssistantBlock, AssistantContent, ThinkingBlock, ToolUseRequest};
///
/// let call = ToolUseRequest {
///     id: "t1".into(),
///     name: "bash".into(),
///     input: serde_json::json!({"command": "ls"}),
/// };
/// let mut content = AssistantContent::new(vec![
///     AssistantBlock::Thinking(ThinkingBlock::Redacted { data: "opaque".into() }),
///     AssistantBlock::Text("listing".into()),
///     AssistantBlock::ToolUse(call),
/// ]);
/// assert_eq!(content.text().as_deref(), Some("listing"));
/// assert_eq!(content.tool_calls().count(), 1);
/// assert_eq!(content.thinking_blocks().count(), 1);
///
/// content.strip_thinking();
/// assert_eq!(content.blocks().len(), 2);
/// ```
#[derive(Debug, Clone, Default)]
pub struct AssistantContent(Vec<AssistantBlock>);

impl AssistantContent {
    /// Content of optional leading text followed by tool calls, with no thinking.
    ///
    /// For providers that do not report block order or thinking, and for tests.
    #[must_use]
    pub fn from_text_and_calls(text: Option<String>, tool_calls: Vec<ToolUseRequest>) -> Self {
        Self(
            text.map(AssistantBlock::Text)
                .into_iter()
                .chain(tool_calls.into_iter().map(AssistantBlock::ToolUse))
                .collect(),
        )
    }

    /// Wrap blocks already in response order.
    #[must_use]
    pub fn new(blocks: Vec<AssistantBlock>) -> Self {
        Self(blocks)
    }

    /// Blocks in response order.
    #[must_use]
    pub fn blocks(&self) -> &[AssistantBlock] {
        &self.0
    }

    /// Consume the content and return the blocks in response order.
    #[must_use]
    pub fn into_blocks(self) -> Vec<AssistantBlock> {
        self.0
    }

    /// All text blocks concatenated, or `None` when the response carried no text.
    #[must_use]
    pub fn text(&self) -> Option<String> {
        let mut parts = self.0.iter().filter_map(|b| match b {
            AssistantBlock::Text(t) => Some(t.as_str()),
            _ => None,
        });
        let first = parts.next()?;
        Some(parts.fold(first.to_owned(), |mut acc, t| {
            acc.push_str(t);
            acc
        }))
    }

    /// Consume the content and return its tool invocation requests in response order.
    #[must_use]
    pub fn into_tool_calls(self) -> Vec<ToolUseRequest> {
        self.0
            .into_iter()
            .filter_map(AssistantBlock::into_tool_use)
            .collect()
    }

    /// Tool invocation requests in response order.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolUseRequest> {
        self.0.iter().filter_map(AssistantBlock::as_tool_use)
    }

    /// Message parts for an assistant message carrying these blocks, in response order.
    #[must_use]
    pub fn to_message_parts(&self) -> Vec<MessagePart> {
        self.0
            .iter()
            .map(|block| match block {
                AssistantBlock::Thinking(t) => MessagePart::from(t),
                AssistantBlock::Text(text) => MessagePart::Text { text: text.clone() },
                AssistantBlock::ToolUse(call) => MessagePart::ToolUse {
                    id: call.id.clone(),
                    name: call.name.to_string(),
                    input: call.input.clone(),
                },
            })
            .collect()
    }

    /// Drop every thinking block, keeping text and tool-use blocks in order.
    ///
    /// For responses whose thinking was produced under a prefix that has since been rewritten.
    pub fn strip_thinking(&mut self) {
        self.0.retain(|b| !matches!(b, AssistantBlock::Thinking(_)));
    }

    /// Thinking blocks in response order.
    pub fn thinking_blocks(&self) -> impl Iterator<Item = &ThinkingBlock> {
        self.0.iter().filter_map(|b| match b {
            AssistantBlock::Thinking(t) => Some(t),
            _ => None,
        })
    }
}

/// Accumulates streamed events into [`AssistantContent`], preserving arrival order.
///
/// Consecutive text chunks merge into one text block; a thinking or tool-use block starts a new
/// entry, so a later text chunk opens a fresh text block. [`begin_text_block`] forces the same
/// split between two adjacent text blocks the provider reported separately.
///
/// [`begin_text_block`]: Self::begin_text_block
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{AssistantBlock, AssistantContentBuilder, ThinkingBlock};
///
/// let mut builder = AssistantContentBuilder::default();
/// builder.push_text("Hel");
/// builder.push_text("lo");
/// builder.push_thinking(ThinkingBlock::Redacted { data: "x".into() });
/// builder.push_text("again");
///
/// let content = builder.build();
/// assert!(matches!(
///     content.blocks(),
///     [AssistantBlock::Text(a), AssistantBlock::Thinking(_), AssistantBlock::Text(b)]
///         if a == "Hello" && b == "again"
/// ));
///
/// let mut builder = AssistantContentBuilder::default();
/// builder.push_text("a");
/// builder.begin_text_block();
/// builder.push_text("b");
/// assert_eq!(builder.build().blocks().len(), 2);
/// ```
#[derive(Debug, Default)]
pub struct AssistantContentBuilder {
    blocks: Vec<AssistantBlock>,
    /// The next text chunk opens a new block even if the last block is text.
    text_break: bool,
}

impl AssistantContentBuilder {
    /// Append a text chunk, extending the trailing text block unless a new block was requested.
    pub fn push_text(&mut self, chunk: &str) {
        if let Some(AssistantBlock::Text(last)) = self.blocks.last_mut()
            && !self.text_break
        {
            last.push_str(chunk);
        } else {
            self.blocks.push(AssistantBlock::Text(chunk.to_owned()));
        }
        self.text_break = false;
    }

    /// Mark the start of a new text block: the next [`push_text`](Self::push_text) does not
    /// merge into a preceding text block.
    pub fn begin_text_block(&mut self) {
        self.text_break = true;
    }

    /// Append a completed thinking block.
    pub fn push_thinking(&mut self, block: ThinkingBlock) {
        self.blocks.push(AssistantBlock::Thinking(block));
    }

    /// Append a completed tool-use request.
    pub fn push_tool_use(&mut self, call: ToolUseRequest) {
        self.blocks.push(AssistantBlock::ToolUse(call));
    }

    /// Finish and return the ordered content.
    #[must_use]
    pub fn build(self) -> AssistantContent {
        AssistantContent::new(self.blocks)
    }
}

/// Response from [`LlmProvider::chat_with_tools`].
///
/// When the model returns `ToolUse`, the caller must:
/// 1. Execute each tool call in `content`.
/// 2. Append an `assistant` message with the blocks of `content`, in order.
/// 3. Append a `user` message containing [`MessagePart::ToolResult`] entries.
/// 4. Call `chat_with_tools` again to continue the conversation.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum ChatResponse {
    /// Model produced text output only.
    Text {
        /// Response text.
        text: String,
        /// Whether the provider kept the thinking blocks sent in the request.
        replay: ThinkingReplay,
        /// Server-side compaction summary produced while serving this very request, if any.
        compaction: Option<String>,
    },
    /// Model requests one or more tool invocations.
    ToolUse {
        /// Thinking, text and tool-use blocks in the order the model emitted them.
        content: AssistantContent,
        /// Whether the provider kept the thinking blocks sent in the request.
        replay: ThinkingReplay,
        /// Server-side compaction summary produced while serving this very request, if any.
        ///
        /// Travels with the response instead of a provider-level slot, so a provider shared
        /// by several conversations (a router, a subagent) cannot hand one conversation the
        /// summary of another.
        compaction: Option<String>,
    },
}

impl ChatResponse {
    /// Build a text-only response without a compaction summary.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::provider::ChatResponse;
    ///
    /// let mut resp = ChatResponse::text("hello");
    /// assert_eq!(resp.take_compaction(), None);
    /// assert!(matches!(resp, ChatResponse::Text { ref text, .. } if text == "hello"));
    /// ```
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            replay: ThinkingReplay::Intact,
            compaction: None,
        }
    }

    /// Content-free one-line description for logs: variant, text bytes, thinking block count
    /// and tool names. Never includes model output.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::provider::ChatResponse;
    ///
    /// let summary = ChatResponse::text("secret").log_summary();
    /// assert!(summary.contains("text_bytes=6"));
    /// assert!(!summary.contains("secret"));
    /// ```
    #[must_use]
    pub fn log_summary(&self) -> String {
        match self {
            Self::Text {
                text,
                replay,
                compaction,
            } => format!(
                "Text(text_bytes={}, replay={replay:?}, compaction={})",
                text.len(),
                compaction.is_some()
            ),
            Self::ToolUse {
                content,
                replay,
                compaction,
            } => {
                let text_bytes = content.text().map_or(0, |t| t.len());
                let tools: Vec<&str> = content.tool_calls().map(|c| c.name.as_str()).collect();
                format!(
                    "ToolUse(text_bytes={text_bytes}, thinking_blocks={}, tools={tools:?}, replay={replay:?}, compaction={})",
                    content.thinking_blocks().count(),
                    compaction.is_some()
                )
            }
        }
    }

    /// Which thinking blocks of the request the provider kept, whatever the response shape.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::ThinkingReplay;
    /// use zeph_llm::provider::ChatResponse;
    ///
    /// let mut resp = ChatResponse::text("hi");
    /// assert_eq!(resp.replay(), ThinkingReplay::Intact);
    /// resp.set_replay(ThinkingReplay::Dropped { first_message_index: 2 });
    /// assert_eq!(resp.replay(), ThinkingReplay::Dropped { first_message_index: 2 });
    /// ```
    #[must_use]
    pub fn replay(&self) -> ThinkingReplay {
        match self {
            Self::Text { replay, .. } | Self::ToolUse { replay, .. } => *replay,
        }
    }

    /// Overwrite the thinking-replay report of the response.
    pub fn set_replay(&mut self, value: ThinkingReplay) {
        match self {
            Self::Text { replay, .. } | Self::ToolUse { replay, .. } => *replay = value,
        }
    }

    /// Drop thinking blocks from a tool-use response; no-op for text responses.
    pub fn strip_thinking(&mut self) {
        if let Self::ToolUse { content, .. } = self {
            content.strip_thinking();
        }
    }

    /// Build a tool-use response from optional leading text and tool calls.
    ///
    /// For providers that do not report block order or thinking, and for tests.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::provider::{ChatResponse, ToolUseRequest};
    ///
    /// let call = ToolUseRequest {
    ///     id: "t1".into(),
    ///     name: "bash".into(),
    ///     input: serde_json::json!({}),
    /// };
    /// let resp = ChatResponse::tool_use(Some("running".into()), vec![call]);
    /// let ChatResponse::ToolUse { content, .. } = resp else { unreachable!() };
    /// assert_eq!(content.text().as_deref(), Some("running"));
    /// assert_eq!(content.tool_calls().count(), 1);
    /// ```
    #[must_use]
    pub fn tool_use(text: Option<String>, tool_calls: Vec<ToolUseRequest>) -> Self {
        Self::ToolUse {
            content: AssistantContent::from_text_and_calls(text, tool_calls),
            replay: ThinkingReplay::Intact,
            compaction: None,
        }
    }

    /// Attach a server-side compaction summary to the response.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::provider::ChatResponse;
    ///
    /// let mut resp = ChatResponse::tool_use(None, vec![]);
    /// resp.attach_compaction("summary".into());
    /// assert_eq!(resp.take_compaction().as_deref(), Some("summary"));
    /// assert_eq!(resp.take_compaction(), None, "taken once");
    ///
    /// let mut text = ChatResponse::text("done");
    /// text.attach_compaction("kept".into());
    /// assert_eq!(text.take_compaction().as_deref(), Some("kept"));
    /// ```
    pub fn attach_compaction(&mut self, summary: String) {
        match self {
            Self::ToolUse { compaction, .. } | Self::Text { compaction, .. } => {
                *compaction = Some(summary);
            }
        }
    }

    /// Take the server-side compaction summary out of the response, if it carries one.
    pub fn take_compaction(&mut self) -> Option<String> {
        match self {
            Self::ToolUse { compaction, .. } | Self::Text { compaction, .. } => compaction.take(),
        }
    }
}

/// Boxed, type-erased future for an [`LlmProvider`] method returning `T`.
///
/// Router-composing implementors ([`crate::router::RouterProvider`],
/// [`crate::router::triage::TriageRouter`]) must return this instead of an opaque
/// `impl Future + Send` from their `LlmProvider` methods. Those methods call back
/// into [`crate::any::AnyProvider`], which can itself hold a nested router — a
/// genuinely recursive call graph, so an opaque return type here is self-referential.
/// The compiler's opaque-type auto-trait check recurses without a useful bound in
/// that case (confirmed: raising `#![recursion_limit]` does not converge — it
/// crashes rustc's trait solver instead of resolving). Boxing to `dyn Future` erases
/// the concrete type at this exact boundary and breaks the cycle. Backends with no
/// recursive call graph (`OllamaProvider`, `ClaudeProvider`, etc.) are unaffected and
/// should keep returning an opaque `impl Future + Send`.
pub type LlmFuture<T> = Pin<Box<dyn Future<Output = Result<T, LlmError>> + Send>>;

/// Boxed future returning an embedding vector, returned by [`EmbedFn`].
pub type EmbedFuture = LlmFuture<Vec<f32>>;

/// A Send + Sync closure that embeds a text slice into a vector.
///
/// Obtain a provider-backed `EmbedFn` via [`crate::any::AnyProvider::embed_fn`].
/// The closure captures an `Arc`-wrapped provider clone, so it is cheap to clone.
pub type EmbedFn = Box<dyn Fn(&str) -> EmbedFuture + Send + Sync>;

/// Sender for emitting human-readable status events (retries, fallbacks) to the UI layer.
///
/// When set on a provider, the provider sends short strings such as
/// `"Retrying after rate limit…"` or `"Falling back to secondary provider"`.
/// The TUI consumes these to show real-time activity spinners.
pub type StatusTx = tokio::sync::mpsc::UnboundedSender<String>;

/// Best-effort fallback for debug dump request payloads when a provider does not expose
/// its concrete API request body.
#[must_use]
pub fn default_debug_request_json(
    messages: &[Message],
    tools: &[ToolDefinition],
) -> serde_json::Value {
    serde_json::json!({
        "model": serde_json::Value::Null,
        "max_tokens": serde_json::Value::Null,
        "messages": serde_json::to_value(messages).unwrap_or(serde_json::Value::Array(vec![])),
        "tools": serde_json::to_value(tools).unwrap_or(serde_json::Value::Array(vec![])),
        "temperature": serde_json::Value::Null,
        "cache_control": serde_json::Value::Null,
    })
}

/// Partial LLM generation parameter overrides for experiment variation injection.
///
/// Applied by the experiment engine to clone-and-patch a provider before evaluation,
/// so each variation is scored with its specific generation parameters.
///
/// Only `Some` fields are applied; `None` fields leave the provider's configured
/// defaults unchanged. Not all providers support all fields — unsupported fields
/// are silently ignored by each backend.
#[derive(Debug, Clone, Default)]
pub struct GenerationOverrides {
    /// Sampling temperature in `[0.0, 2.0]`. Lower = more deterministic.
    pub temperature: Option<f64>,
    /// Nucleus sampling probability in `[0.0, 1.0]`.
    pub top_p: Option<f64>,
    /// Top-K sampling cutoff (number of top tokens to consider).
    pub top_k: Option<usize>,
    /// Penalty for tokens that have already appeared (OpenAI-compatible providers).
    pub frequency_penalty: Option<f64>,
    /// Penalty for topics the model has already covered (OpenAI-compatible providers).
    pub presence_penalty: Option<f64>,
}

/// Message role in a conversation.
///
/// Determines how each message is presented to the model:
/// - `System` — global instructions prepended before the conversation
/// - `User` — human turn input
/// - `Assistant` — previous model output (used for multi-turn context)
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// A typed content part within a [`Message`].
///
/// Messages may contain zero or more parts that represent heterogeneous content:
/// plain text, tool invocations, memory recall fragments, images, and internal
/// protocol blocks (thinking, compaction). Most providers flatten these into a single
/// string before sending; Claude encodes them as structured content blocks.
///
/// # Ordering invariants
///
/// - `ToolUse` parts must precede their corresponding `ToolResult` parts.
/// - `ThinkingBlock` / `RedactedThinkingBlock` parts must be preserved verbatim in
///   multi-turn requests so the API can correctly attribute reasoning.
/// - `Compaction` parts must be preserved verbatim; the API uses them to prune
///   prior history on subsequent turns (Claude compact-2026-01-12 beta).
#[non_exhaustive]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessagePart {
    /// Plain assistant or user text.
    Text { text: String },
    /// Output from a tool execution, optionally compacted.
    ToolOutput {
        tool_name: zeph_common::ToolName,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted_at: Option<i64>,
    },
    /// Memory recall fragment injected by the agent's semantic memory layer.
    Recall { text: String },
    /// Repository or file code context injected by the code indexing layer.
    CodeContext { text: String },
    /// Compaction summary replacing pruned conversation history.
    Summary { text: String },
    /// Cross-session memory fragment carried over from a previous conversation.
    CrossSession { text: String },
    /// Model-initiated tool invocation. Pairs with a subsequent [`MessagePart::ToolResult`].
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Tool execution result returned to the model after a [`MessagePart::ToolUse`].
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// Inline image payload (vision input).
    Image(Box<ImageData>),
    /// Claude thinking block — must be preserved verbatim in multi-turn requests.
    ThinkingBlock { thinking: String, signature: String },
    /// Claude redacted thinking block — preserved as-is in multi-turn requests.
    RedactedThinkingBlock { data: String },
    /// Claude server-side compaction block — must be preserved verbatim in multi-turn requests
    /// so the API can correctly prune prior history on the next turn.
    Compaction { summary: String },
    /// Agent-authored per-iteration state (remaining tool budget, LSP notes, utility hints).
    ///
    /// Attached only to the unsent tail `User` message that carries `ToolResult` parts (at most
    /// one per message) via [`Message::append_runtime_note`], so volatile state is appended to
    /// the request instead of inserted before the cached prefix. It is never part of
    /// `Message::content`, never user-visible, and never persisted
    /// ([`MessagePart::strip_for_persistence`]); provider request builders send it through
    /// [`MessagePart::as_request_text`] / [`Message::to_request_text`].
    RuntimeNote { text: String },
    /// Per-turn context the agent attaches to a turn input in the `append` context layout
    /// (retrieved memory/code, delta directives).
    ///
    /// Like [`MessagePart::RuntimeNote`] it is never part of `Message::content`, never
    /// user-visible, never persisted and never fed to a summarizer; request builders send it
    /// through [`Message::to_request_text`] or render it position-aware.
    ///
    /// Never deserialized: a stored or imported `parts_json` must not be able to materialize a
    /// directive that the request builders would send with system authority.
    #[serde(skip_deserializing)]
    TurnContext(TurnContextBlock),
}

/// Role a [`TurnContextBlock`] plays in the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TurnContextKind {
    /// Operator-authority directives (skill bodies, environment, notices, budget, time).
    Directives,
    /// Retrieved data (memory, code index, graph); carries no instruction authority.
    Retrieved,
}

/// Content-trust tier of a [`TurnContextBlock`], the minimum over its sections.
///
/// Mirrors `zeph_sanitizer::ContentTrustLevel` (which this crate cannot depend on); the
/// discriminants are identical so the two convert through `u8`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
#[repr(u8)]
pub enum TurnContextTrust {
    /// System-authored or direct user content.
    Trusted = 0,
    /// Output of local executors.
    LocalUntrusted = 1,
    /// External sources: web, MCP, A2A, memory retrieval.
    ExternalUntrusted = 2,
}

impl TurnContextTrust {
    /// The `u8` discriminant, identical to `zeph_sanitizer::ContentTrustLevel as u8`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::provider::TurnContextTrust;
    ///
    /// assert_eq!(TurnContextTrust::ExternalUntrusted.as_u8(), 2);
    /// ```
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`as_u8`](Self::as_u8); `None` for an unknown discriminant.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::provider::TurnContextTrust;
    ///
    /// assert_eq!(TurnContextTrust::from_u8(1), Some(TurnContextTrust::LocalUntrusted));
    /// assert_eq!(TurnContextTrust::from_u8(9), None);
    /// ```
    #[must_use]
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Trusted),
            1 => Some(Self::LocalUntrusted),
            2 => Some(Self::ExternalUntrusted),
            _ => None,
        }
    }

    /// Like [`from_u8`](Self::from_u8) but fails closed: an unknown discriminant is treated as
    /// the least trusted tier, so it can never gain system authority.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::provider::TurnContextTrust;
    ///
    /// assert_eq!(TurnContextTrust::from_u8_or_untrusted(0), TurnContextTrust::Trusted);
    /// assert_eq!(TurnContextTrust::from_u8_or_untrusted(200), TurnContextTrust::ExternalUntrusted);
    /// ```
    #[must_use]
    pub fn from_u8_or_untrusted(value: u8) -> Self {
        Self::from_u8(value).unwrap_or(Self::ExternalUntrusted)
    }
}

/// Content hash of one retrieved item, used to avoid re-injecting an item that is already
/// present in the live history (item-level delta).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RetrievalKey(pub u64);

/// One block of per-turn context attached to a turn input. See [`MessagePart::TurnContext`].
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{
///     Message, MessagePart, RetrievalKey, Role, TurnContextBlock, TurnContextKind,
///     TurnContextTrust,
/// };
///
/// let block = TurnContextBlock {
///     kind: TurnContextKind::Retrieved,
///     trust: TurnContextTrust::ExternalUntrusted,
///     text: "recalled fact".to_owned(),
///     keys: vec![RetrievalKey(1)],
/// };
/// let msg = Message::from_parts(
///     Role::User,
///     vec![MessagePart::TurnContext(block), MessagePart::Text { text: "hi".to_owned() }],
/// );
/// assert_eq!(msg.content, "hi");
/// assert_eq!(msg.to_request_text(), "recalled fact\nhi");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TurnContextBlock {
    /// Whether the block carries directives or retrieved data.
    ///
    /// Serialized as `context_kind`: `kind` is the tag of [`MessagePart`] and a duplicate key
    /// would make the JSON ambiguous.
    #[serde(rename = "context_kind")]
    pub kind: TurnContextKind,
    /// Minimum trust tier over the block's sections.
    pub trust: TurnContextTrust,
    /// Rendered, sanitized text sent to the provider.
    pub text: String,
    /// Keys of the retrieved items contained in `text`.
    pub keys: Vec<RetrievalKey>,
}

/// Whether the provider reuses a cached request prefix across calls.
///
/// Drives the `auto` context layout: a [`Prefix`](Self::Prefix) provider benefits from an
/// append-only request, others do not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PrefixCache {
    /// No prefix cache (or none worth preserving).
    #[default]
    None,
    /// The provider caches the request prefix; rewriting history invalidates it.
    Prefix {
        /// How long an idle cache entry survives; `None` when the provider cannot say.
        ttl: Option<std::time::Duration>,
    },
}

impl PrefixCache {
    /// `Prefix` if either side is `Prefix`; used by routers that hold several providers.
    ///
    /// Which member serves a request is not known up front, so a wrong TTL must not make the
    /// caller rewrite a cache that is still alive: two known TTLs keep the longer one, and a
    /// known TTL next to an unknown one becomes unknown.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zeph_llm::provider::PrefixCache;
    ///
    /// let short = PrefixCache::Prefix { ttl: Some(Duration::from_mins(5)) };
    /// let long = PrefixCache::Prefix { ttl: Some(Duration::from_hours(1)) };
    /// let unknown = PrefixCache::Prefix { ttl: None };
    /// assert_eq!(PrefixCache::None.or(short), short);
    /// assert_eq!(long.or(short), long);
    /// assert_eq!(short.or(unknown), unknown);
    /// assert_eq!(PrefixCache::None.or(PrefixCache::None), PrefixCache::None);
    /// ```
    #[must_use]
    pub fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::None, o) | (o, Self::None) => o,
            (Self::Prefix { ttl: a }, Self::Prefix { ttl: b }) => Self::Prefix {
                ttl: a.zip(b).map(|(a, b)| a.max(b)),
            },
        }
    }

    /// Idle lifetime of the cached prefix, if the provider caches one and reports a TTL.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zeph_llm::provider::PrefixCache;
    ///
    /// let cache = PrefixCache::Prefix { ttl: Some(Duration::from_mins(5)) };
    /// assert_eq!(cache.ttl(), Some(Duration::from_mins(5)));
    /// assert_eq!(PrefixCache::None.ttl(), None);
    /// ```
    #[must_use]
    pub fn ttl(self) -> Option<std::time::Duration> {
        match self {
            Self::None => None,
            Self::Prefix { ttl } => ttl,
        }
    }

    /// Whether the provider caches the request prefix.
    #[must_use]
    pub fn is_prefix(self) -> bool {
        matches!(self, Self::Prefix { .. })
    }
}

/// Which assistant messages keep their thinking blocks in a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ThinkingScope {
    /// Forward thinking from every assistant message unchanged.
    Verbatim,
    /// Forward thinking only from assistant messages after the latest user turn input.
    ///
    /// Models with preserved thinking bind each block to its prefix; blocks from earlier turns
    /// would be dropped by the provider anyway, and sending them would break prefix stability.
    /// Without a turn-input marker nothing can be scoped and thinking is forwarded unchanged.
    CurrentTurn,
}

/// Identity of one conversation, so per-conversation provider state (the prefix guard) is not
/// shared between conversations that use the same provider instance.
///
/// A fresh random value is generated whenever the layout is pinned (session start, `/clear`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationKey(pub u64);

/// Per-request policy the agent stamps on `messages[0]` right before a provider call.
///
/// Absent policy means today's behavior: the provider picks its [`ThinkingScope`] from the model
/// and runs no prefix guard.
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{ConversationKey, RequestPolicy, ThinkingScope};
///
/// let policy = RequestPolicy {
///     thinking: ThinkingScope::Verbatim,
///     conversation: ConversationKey(7),
/// };
/// assert_eq!(policy.conversation, ConversationKey(7));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestPolicy {
    /// Thinking forwarding scope for this request.
    pub thinking: ThinkingScope,
    /// Conversation this request belongs to.
    pub conversation: ConversationKey,
}

impl MessagePart {
    /// Return the plain text content if this part is a text-like variant (`Text`, `Recall`,
    /// `CodeContext`, `Summary`, `CrossSession`), `None` otherwise.
    #[must_use]
    pub fn as_plain_text(&self) -> Option<&str> {
        match self {
            Self::Text { text }
            | Self::Recall { text }
            | Self::CodeContext { text }
            | Self::Summary { text }
            | Self::CrossSession { text } => Some(text.as_str()),
            _ => None,
        }
    }

    /// Like [`as_plain_text`](Self::as_plain_text) but also yields the text of a
    /// [`MessagePart::RuntimeNote`]. Request builders use this; display and persistence paths
    /// must keep using `as_plain_text` so notes stay invisible to them.
    #[must_use]
    pub fn as_request_text(&self) -> Option<&str> {
        match self {
            Self::RuntimeNote { text } => Some(text.as_str()),
            other => other.as_plain_text(),
        }
    }

    /// Return the image data if this part is an `Image` variant, `None` otherwise.
    #[must_use]
    pub fn as_image(&self) -> Option<&ImageData> {
        if let Self::Image(img) = self {
            Some(img)
        } else {
            None
        }
    }

    /// Return a cloned copy of `parts` with every `Image` entry removed.
    ///
    /// `Image` parts are ephemeral, current-turn-only vision input (spec-072 §4, C1) and must
    /// never reach a persistence sink — `SQLite` `parts_json`, the Qdrant embed path, the durable
    /// JSONL session log, or a sub-agent transcript file. Callers apply this once, immediately
    /// above the persistence write, leaving the in-memory slice used for the current turn's
    /// provider request untouched.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::provider::{ImageData, MessagePart};
    ///
    /// let parts = vec![
    ///     MessagePart::Text {
    ///         text: "hello".to_owned(),
    ///     },
    ///     MessagePart::Image(Box::new(ImageData {
    ///         data: vec![0xFF, 0xD8, 0xFF, 0xE0],
    ///         mime_type: "image/jpeg".to_owned(),
    ///     })),
    /// ];
    /// let stripped = MessagePart::strip_images(&parts);
    /// assert_eq!(stripped.len(), 1);
    /// assert!(!stripped.iter().any(|p| matches!(p, MessagePart::Image(_))));
    /// ```
    #[must_use]
    pub fn strip_images(parts: &[MessagePart]) -> Vec<MessagePart> {
        parts
            .iter()
            .filter(|p| !matches!(p, MessagePart::Image(_)))
            .cloned()
            .collect()
    }

    /// Return a cloned copy of `parts` without anything that must never reach a persistence
    /// sink: `Image` payloads (see [`strip_images`](Self::strip_images)),
    /// [`MessagePart::RuntimeNote`] and [`MessagePart::TurnContext`] (per-iteration/per-turn
    /// state that is stale on restore), and thinking blocks (bound to the live conversation prefix, so useless after a restore, and
    /// unscrubbed reasoning text).
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::provider::MessagePart;
    ///
    /// let parts = vec![
    ///     MessagePart::Text { text: "hello".to_owned() },
    ///     MessagePart::RuntimeNote { text: "budget".to_owned() },
    /// ];
    /// let stripped = MessagePart::strip_for_persistence(&parts);
    /// assert_eq!(stripped.len(), 1);
    /// ```
    #[must_use]
    pub fn strip_for_persistence(parts: &[MessagePart]) -> Vec<MessagePart> {
        parts
            .iter()
            .filter(|p| {
                !matches!(
                    p,
                    MessagePart::Image(_)
                        | MessagePart::RuntimeNote { .. }
                        | MessagePart::TurnContext(_)
                        | MessagePart::ThinkingBlock { .. }
                        | MessagePart::RedactedThinkingBlock { .. }
                )
            })
            .cloned()
            .collect()
    }
}

/// Outcome of [`Message::append_runtime_note`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NoteAppend {
    /// The note was added as the last part.
    Appended,
    /// The message already carries a note; nothing changed (at most one per message).
    AlreadyPresent,
    /// The message is not a `User` message with `ToolResult` parts; nothing changed.
    NotAToolResultMessage,
}

#[derive(Clone, Serialize, Deserialize)]
/// Raw image payload for vision-capable providers.
///
/// The `data` field is serialized as a Base64 string. `mime_type` must be a valid
/// image MIME type supported by the target provider (e.g. `"image/png"`, `"image/jpeg"`).
pub struct ImageData {
    #[serde(with = "serde_bytes_base64")]
    pub data: Vec<u8>,
    pub mime_type: String,
}

impl std::fmt::Debug for ImageData {
    /// Redacts the raw image bytes — only the MIME type and byte count are printed.
    ///
    /// `data` can carry arbitrary externally-sourced bytes (MCP tool results, user uploads);
    /// a derived `Debug` would dump the full payload into logs/panics.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[image: {}, {} bytes]", self.mime_type, self.data.len())
    }
}

mod serde_bytes_base64 {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        STANDARD.decode(&s).map_err(serde::de::Error::custom)
    }
}

/// Visibility of a message to agent and user.
///
/// Replaces the former `(agent_visible: bool, user_visible: bool)` pair. Only
/// [`Marker`](MessageVisibility::Marker) is hidden from both consumers, and it is explicit.
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::MessageVisibility;
///
/// let v = MessageVisibility::AgentOnly;
/// assert!(v.is_agent_visible());
/// assert!(!v.is_user_visible());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MessageVisibility {
    /// Visible to both the agent (LLM context) and the user (conversation log).
    Both,
    /// Visible to the agent only (e.g. compaction summaries, internal context).
    AgentOnly,
    /// Visible to the user only (e.g. compacted originals shown in history).
    UserOnly,
    /// Bookkeeping marker (e.g. a focus checkpoint): visible to neither the agent nor the user.
    /// Kept in the in-memory history only so the agent loop can find it again.
    Marker,
}

impl MessageVisibility {
    /// Returns `true` if this message should be included in the LLM request context.
    #[must_use]
    pub fn is_agent_visible(self) -> bool {
        matches!(self, MessageVisibility::Both | MessageVisibility::AgentOnly)
    }

    /// Returns `true` if this message should appear in the user-facing conversation log.
    #[must_use]
    pub fn is_user_visible(self) -> bool {
        matches!(self, MessageVisibility::Both | MessageVisibility::UserOnly)
    }

    /// Every variant, for building visibility predicates from the type instead of by hand.
    pub const ALL: [Self; 4] = [Self::Both, Self::AgentOnly, Self::UserOnly, Self::Marker];
}

impl Default for MessageVisibility {
    /// Defaults to [`Both`](MessageVisibility::Both) — visible to agent and user.
    fn default() -> Self {
        MessageVisibility::Both
    }
}

impl MessageVisibility {
    /// Serialize to the SQLite/PostgreSQL text value stored in the `visibility` column.
    #[must_use]
    pub fn as_db_str(self) -> &'static str {
        match self {
            MessageVisibility::Both => "both",
            MessageVisibility::AgentOnly => "agent_only",
            MessageVisibility::UserOnly => "user_only",
            MessageVisibility::Marker => "marker",
        }
    }

    /// Deserialize from the SQLite/PostgreSQL text value stored in the `visibility` column.
    ///
    /// Unknown values (e.g. from a future migration) default to `Both` for safety.
    #[must_use]
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "agent_only" => MessageVisibility::AgentOnly,
            "user_only" => MessageVisibility::UserOnly,
            "marker" => MessageVisibility::Marker,
            _ => MessageVisibility::Both,
        }
    }
}

/// Why a message exists in the history, beyond its role.
///
/// Lets history-rewriting passes (server compaction, floor computation) identify
/// messages without relying on positional heuristics such as "last `User` message",
/// which mid-tool-loop is a tool-result carrier rather than the user's input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MessageOrigin {
    /// The user's input that started the current turn (or a queued follow-up injected into it).
    TurnInput,
    /// Context injected at the head of history (recall, summaries, cross-session, digest) by
    /// [`insert_at_context_floor`]. Re-injected every turn, so it is never subgoal-tagged,
    /// fidelity-scored, or selected for compression.
    HeadInjection,
}

/// Per-message visibility and metadata controlling agent context and user display.
///
/// Constructors [`agent_only`](Self::agent_only), [`user_only`](Self::user_only),
/// and [`focus_pinned`](Self::focus_pinned) cover the most common combinations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageMetadata {
    /// Who can see this message.
    pub visibility: MessageVisibility,
    /// Unix timestamp (seconds) when this message was compacted, if applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_at: Option<i64>,
    /// Pre-computed tool pair summary, applied lazily when context pressure rises.
    /// Stored on the tool response message; cleared after application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_summary: Option<String>,
    /// When true, this message is excluded from all compaction passes (soft pruning,
    /// hard summarization, sidequest eviction). Used for the Focus Knowledge block (#1850).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub focus_pinned: bool,
    /// Unique marker UUID set when `start_focus` begins a session. Used by `complete_focus`
    /// to locate the checkpoint without relying on a fragile raw index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_marker_id: Option<uuid::Uuid>,
    /// `SQLite` row ID for this message. Populated when loading from DB or after persisting.
    /// Never serialized — always re-populated from the database on load.
    #[serde(skip)]
    pub db_id: Option<i64>,
    /// Fidelity level assigned by `FidelityScorer` during context assembly.
    ///
    /// `None` when fidelity scoring is disabled or the message has not yet been scored.
    /// Used for debug tracing and compaction input filtering (INV-02).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fidelity_tag: Option<zeph_common::ContextFidelity>,
    /// Cached embedding vector for semantic fidelity scoring.
    ///
    /// In-memory only — not serialized or persisted to the database.
    #[serde(skip)]
    pub embedding: Option<Vec<f32>>,
    /// Write-time memory-consent gate content-trust tier (issue #6490 `MemGhost`, #6558 fix).
    ///
    /// Raw `u8` discriminant of `zeph_sanitizer::ContentTrustLevel` (kept as a plain integer
    /// here, not the enum itself, since `zeph-llm` does not depend on `zeph-sanitizer` — mirrors
    /// `zeph_core::memory_tools::MemoryConsentTrustSlot`'s same representation choice). Set on
    /// tool-result batch messages by `Agent::process_tool_result_batch` to the batch's
    /// worst-case trust tier. Scanned by `Agent::context_max_trust_level` so untrusted content
    /// is still recognized by the memory-consent gate for as long as it remains in the live
    /// conversation context — including across a user-turn boundary — rather than only for the
    /// single turn in which it was fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_level: Option<u8>,
    /// Provenance marker for messages the agent loop must be able to find again after
    /// history is rewritten (for example the user turn input kept by server compaction).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    /// Subgoal this message belongs to (`Subgoal`/`SubgoalMig` pruning strategies).
    ///
    /// In-memory only — never serialized. Membership lives on the message so it follows the
    /// message through drains, reinserts, and deferred-summary insertions.
    #[serde(skip)]
    pub subgoal_id: Option<zeph_common::SubgoalId>,
    /// Per-request provider policy, stamped on `messages[0]` right before a provider call.
    ///
    /// In-memory only — never serialized.
    #[serde(skip)]
    pub request_policy: Option<RequestPolicy>,
}

impl Default for MessageMetadata {
    fn default() -> Self {
        Self {
            visibility: MessageVisibility::Both,
            compacted_at: None,
            deferred_summary: None,
            focus_pinned: false,
            focus_marker_id: None,
            db_id: None,
            fidelity_tag: None,
            embedding: None,
            trust_level: None,
            origin: None,
            subgoal_id: None,
            request_policy: None,
        }
    }
}

impl MessageMetadata {
    /// Message visible only to the agent (e.g. compaction summary).
    #[must_use]
    pub fn agent_only() -> Self {
        Self {
            visibility: MessageVisibility::AgentOnly,
            ..Self::default()
        }
    }

    /// Message visible only to the user (e.g. compacted original).
    #[must_use]
    pub fn user_only() -> Self {
        Self {
            visibility: MessageVisibility::UserOnly,
            ..Self::default()
        }
    }

    /// Bookkeeping marker hidden from both the agent and the user (e.g. a focus checkpoint).
    #[must_use]
    pub fn marker() -> Self {
        Self {
            visibility: MessageVisibility::Marker,
            ..Self::default()
        }
    }

    /// Pinned Knowledge block — excluded from all compaction passes.
    #[must_use]
    pub fn focus_pinned() -> Self {
        Self {
            visibility: MessageVisibility::AgentOnly,
            focus_pinned: true,
            ..Self::default()
        }
    }
}

/// A single message in a conversation.
///
/// Each message has a [`Role`], a flat `content` string (used when sending to providers
/// that do not support structured parts), and an optional list of [`MessagePart`]s for
/// providers that accept heterogeneous content blocks (e.g. Claude).
///
/// The `content` field is kept in sync with `parts` via [`Message::rebuild_content`].
/// When building messages from structured parts, always use [`Message::from_parts`] —
/// it populates both `parts` and `content`.
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{Message, MessagePart, Role};
///
/// // Simple text-only message
/// let msg = Message::from_legacy(Role::User, "What is Rust?");
/// assert_eq!(msg.to_llm_content(), "What is Rust?");
///
/// // Structured message with parts
/// let parts = vec![
///     MessagePart::Text { text: "Explain this code.".into() },
/// ];
/// let msg = Message::from_parts(Role::User, parts);
/// assert!(!msg.parts.is_empty());
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    /// Flat text representation of this message, derived from `parts` when structured.
    pub content: String,
    #[serde(default)]
    pub parts: Vec<MessagePart>,
    #[serde(default)]
    pub metadata: MessageMetadata,
}

/// Agent-visible projection of a message slice.
///
/// Messages hidden from the agent (`MessageVisibility::UserOnly`, e.g. originals replaced by a
/// deferred tool-pair summary) must never reach a provider request. Every request builder
/// converts through this type, whose only constructors filter, so a builder cannot forget the
/// filter and an adjacency scan (tool-pair repair) never straddles a hidden message.
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{AgentVisible, Message, MessageMetadata, Role};
///
/// let mut hidden = Message::from_legacy(Role::User, "hidden original");
/// hidden.metadata = MessageMetadata::user_only();
/// let messages = vec![Message::from_legacy(Role::System, "sys"), hidden];
///
/// let visible = AgentVisible::new(&messages);
/// assert_eq!(visible.len(), 1);
/// assert_eq!(visible.get(0).unwrap().content, "sys");
/// ```
#[derive(Debug, Clone)]
pub struct AgentVisible<'a>(Vec<&'a Message>);

impl<'a> AgentVisible<'a> {
    /// Keep only the messages whose visibility includes the agent.
    #[must_use]
    pub fn new(messages: &'a [Message]) -> Self {
        Self(
            messages
                .iter()
                .filter(|m| m.metadata.visibility.is_agent_visible())
                .collect(),
        )
    }

    /// Iterate over the visible messages in order.
    #[allow(clippy::must_use_candidate)]
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &'a Message> + '_ {
        self.0.iter().copied()
    }

    /// Number of visible messages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no message is visible.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The visible message at `index` (index into the filtered view).
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&'a Message> {
        self.0.get(index).copied()
    }

    /// Nearest non-`System` visible message after `at`; see
    /// [`next_non_system`](crate::tool_pairing::next_non_system).
    #[must_use]
    pub fn next_non_system(&self, at: usize) -> Option<&'a Message> {
        crate::tool_pairing::next_non_system(&self.0, at).copied()
    }

    /// Nearest non-`System` visible message before `at`; see
    /// [`prev_non_system`](crate::tool_pairing::prev_non_system).
    #[must_use]
    pub fn prev_non_system(&self, at: usize) -> Option<&'a Message> {
        crate::tool_pairing::prev_non_system(&self.0, at).copied()
    }
}

impl<'a> From<&'a [Message]> for AgentVisible<'a> {
    fn from(messages: &'a [Message]) -> Self {
        Self::new(messages)
    }
}

impl<'a> From<&'a Vec<Message>> for AgentVisible<'a> {
    fn from(messages: &'a Vec<Message>) -> Self {
        Self::new(messages)
    }
}

impl<'a, const N: usize> From<&'a [Message; N]> for AgentVisible<'a> {
    fn from(messages: &'a [Message; N]) -> Self {
        Self::new(messages)
    }
}

/// The user message that carries the request the current turn is answering.
///
/// Prefers the message marked [`MessageOrigin::TurnInput`]; without one (subagents, scheduled
/// loops, restored history) falls back to the last `User` message that is neither a tool-result
/// carrier nor a head injection (recall, session digest). Mid-tool-loop the last `User` message is a tool-result carrier whose content is tool
/// output (and may carry a runtime note), so classifiers that need the user's query must not
/// read it by position.
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{Message, MessagePart, Role, turn_query_message};
///
/// let msgs = vec![
///     Message::from_legacy(Role::User, "fix the bug"),
///     Message::from_parts(Role::Assistant, vec![MessagePart::ToolUse {
///         id: "t1".into(),
///         name: "bash".into(),
///         input: serde_json::json!({}),
///     }]),
///     Message::from_parts(Role::User, vec![MessagePart::ToolResult {
///         tool_use_id: "t1".into(),
///         content: "output".into(),
///         is_error: false,
///     }]),
/// ];
/// assert_eq!(turn_query_message(&msgs).unwrap().content, "fix the bug");
/// ```
#[must_use]
pub fn turn_query_message(messages: &[Message]) -> Option<&Message> {
    messages
        .iter()
        .rfind(|m| m.metadata.origin == Some(MessageOrigin::TurnInput))
        .or_else(|| {
            messages.iter().rfind(|m| {
                m.role == Role::User
                    && m.metadata.origin != Some(MessageOrigin::HeadInjection)
                    && !m
                        .parts
                        .iter()
                        .any(|p| matches!(p, MessagePart::ToolResult { .. }))
            })
        })
}

/// Remove thinking blocks from `messages[from..]` and return how many messages changed.
///
/// A thinking block is bound to the conversation prefix it was produced under. When history is
/// edited at index `from` (prune, summary insertion, compaction, focus rewrite), every thinking
/// block after the edit has a stale binding; stripping them once, as a history mutation, keeps
/// later requests append-only instead of letting the provider drop the blocks on every call.
/// Blocks before `from` are untouched.
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{Message, MessagePart, Role, strip_thinking_from};
///
/// let thinking = || MessagePart::ThinkingBlock {
///     thinking: "plan".into(),
///     signature: "sig".into(),
/// };
/// let text = || MessagePart::Text { text: "hi".into() };
/// let mut msgs = vec![
///     Message::from_parts(Role::Assistant, vec![thinking(), text()]),
///     Message::from_parts(Role::Assistant, vec![thinking(), text()]),
/// ];
/// assert_eq!(strip_thinking_from(&mut msgs, 1), 1);
/// assert_eq!(msgs[0].parts.len(), 2);
/// assert_eq!(msgs[1].parts.len(), 1);
/// ```
pub fn strip_thinking_from(messages: &mut [Message], from: usize) -> usize {
    let mut changed = 0;
    for msg in messages.iter_mut().skip(from) {
        let before = msg.parts.len();
        msg.parts.retain(|p| {
            !matches!(
                p,
                MessagePart::ThinkingBlock { .. } | MessagePart::RedactedThinkingBlock { .. }
            )
        });
        if msg.parts.len() != before {
            msg.rebuild_content();
            changed += 1;
        }
    }
    changed
}

/// Whether thinking blocks produced by a provider are cryptographically bound to the
/// conversation prefix they were generated under.
///
/// Claude signs thinking blocks over the prefix, so a history edit invalidates every later
/// block ([`PrefixBound`](Self::PrefixBound)); providers that replay thinking without such a
/// binding keep their blocks across edits ([`Unbound`](Self::Unbound)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ThinkingBinding {
    /// Thinking blocks are signed against the prefix; a history edit invalidates later blocks.
    PrefixBound,
    /// Thinking blocks survive history edits.
    #[default]
    Unbound,
}

/// Strip thinking blocks from `messages[from..]` after a history edit at `from`, but only for
/// providers whose thinking is [`ThinkingBinding::PrefixBound`]. Returns how many messages
/// changed.
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{
///     Message, MessagePart, Role, ThinkingBinding, strip_thinking_after_edit,
/// };
///
/// let mut msgs = vec![Message::from_parts(
///     Role::Assistant,
///     vec![MessagePart::RedactedThinkingBlock { data: "d".into() }],
/// )];
/// assert_eq!(strip_thinking_after_edit(&mut msgs, 0, ThinkingBinding::Unbound), 0);
/// assert_eq!(msgs[0].parts.len(), 1);
/// assert_eq!(strip_thinking_after_edit(&mut msgs, 0, ThinkingBinding::PrefixBound), 1);
/// ```
pub fn strip_thinking_after_edit(
    messages: &mut [Message],
    from: usize,
    binding: ThinkingBinding,
) -> usize {
    match binding {
        ThinkingBinding::PrefixBound => strip_thinking_from(messages, from),
        ThinkingBinding::Unbound => 0,
    }
}

/// Index of the first message that history-head passes may touch.
///
/// Returns `1 +` the index of the last message carrying a [`MessagePart::Compaction`] part, or
/// `1` when there is none (index 0 is the system prompt). Everything below the floor is the
/// system prompt plus a server compaction block and must never be drained, reordered, or
/// have messages inserted in front of it.
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{Message, MessagePart, Role, context_floor};
///
/// let msgs = vec![
///     Message::from_legacy(Role::System, "sys"),
///     Message::from_parts(Role::Assistant, vec![MessagePart::Compaction { summary: "s".into() }]),
///     Message::from_legacy(Role::User, "hi"),
/// ];
/// assert_eq!(context_floor(&msgs), 2);
/// assert_eq!(context_floor(&msgs[..1]), 1);
/// ```
#[must_use]
pub fn context_floor(messages: &[Message]) -> usize {
    messages
        .iter()
        .rposition(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::Compaction { .. }))
        })
        .map_or(1, |i| i + 1)
}

/// Insert `msg` at the head of the rewritable history, directly after [`context_floor`].
///
/// This is the single entry point for "inject right after the system prompt" operations
/// (recall, summaries, cross-session context, focus knowledge block) so they never land in
/// front of a server compaction block. The message is marked
/// [`MessageOrigin::HeadInjection`] unless it already carries an origin.
///
/// # Examples
///
/// ```
/// use zeph_llm::provider::{Message, MessagePart, Role, insert_at_context_floor};
///
/// let mut msgs = vec![
///     Message::from_legacy(Role::System, "sys"),
///     Message::from_parts(Role::Assistant, vec![MessagePart::Compaction { summary: "s".into() }]),
/// ];
/// insert_at_context_floor(&mut msgs, Message::from_legacy(Role::System, "recall"));
/// assert_eq!(msgs[2].content, "recall");
/// ```
pub fn insert_at_context_floor(messages: &mut Vec<Message>, mut msg: Message) {
    msg.metadata
        .origin
        .get_or_insert(MessageOrigin::HeadInjection);
    let floor = context_floor(messages).min(messages.len());
    messages.insert(floor, msg);
}

impl Default for Message {
    fn default() -> Self {
        Self {
            role: Role::User,
            content: String::new(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }
}

impl Message {
    /// Create a simple text-only message without structured parts.
    ///
    /// Use this constructor for system prompts, plain user turns, and assistant
    /// messages produced by providers that return a raw string.
    #[must_use]
    pub fn from_legacy(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    /// Create a message from structured parts, deriving the flat `content` automatically.
    ///
    /// Prefer this constructor when the message contains tool invocations, images,
    /// or other non-text content that providers need to render as separate content blocks.
    #[must_use]
    pub fn from_parts(role: Role, parts: Vec<MessagePart>) -> Self {
        let content = Self::flatten_parts(&parts);
        Self {
            role,
            content,
            parts,
            metadata: MessageMetadata::default(),
        }
    }

    /// Return the flat text content of this message, suitable for providers that do
    /// not support structured content blocks.
    #[must_use]
    pub fn to_llm_content(&self) -> &str {
        &self.content
    }

    /// Texts of this message's [`MessagePart::TurnContext`] blocks of the given kind, in order.
    pub fn turn_context_texts(&self, kind: TurnContextKind) -> impl Iterator<Item = &str> + Clone {
        self.parts.iter().filter_map(move |p| match p {
            MessagePart::TurnContext(b) if b.kind == kind => Some(b.text.as_str()),
            _ => None,
        })
    }

    /// Text for builders that render plain text parts themselves (for example next to images):
    /// the [`TurnContextKind::Retrieved`] texts, the plain text parts joined without a
    /// separator, then the [`TurnContextKind::Directives`] texts, joined by newlines.
    pub(crate) fn plain_text_with_turn_context(&self) -> String {
        let plain: String = self
            .parts
            .iter()
            .filter_map(MessagePart::as_plain_text)
            .collect();
        let mut out = String::new();
        let pieces = self
            .turn_context_texts(TurnContextKind::Retrieved)
            .chain(std::iter::once(plain.as_str()))
            .chain(self.turn_context_texts(TurnContextKind::Directives));
        for piece in pieces.filter(|p| !p.is_empty()) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(piece);
        }
        out
    }

    /// Text a content-based request builder sends for this message: the text of every
    /// [`TurnContextKind::Retrieved`] block, then `content`, then every
    /// [`TurnContextKind::Directives`] block, then every [`MessagePart::RuntimeNote`], joined by
    /// newlines. Borrows `content` when the message carries none of them.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::provider::{Message, MessagePart, Role};
    ///
    /// let mut msg = Message::from_parts(
    ///     Role::User,
    ///     vec![MessagePart::ToolResult {
    ///         tool_use_id: "t1".into(),
    ///         content: "ok".into(),
    ///         is_error: false,
    ///     }],
    /// );
    /// let before = msg.content.clone();
    /// msg.append_runtime_note("<runtime-note>x</runtime-note>".into());
    /// assert_eq!(msg.content, before);
    /// assert!(msg.to_request_text().ends_with("<runtime-note>x</runtime-note>"));
    /// ```
    #[must_use]
    pub fn to_request_text(&self) -> std::borrow::Cow<'_, str> {
        let retrieved = self.turn_context_texts(TurnContextKind::Retrieved);
        let directives = self.turn_context_texts(TurnContextKind::Directives);
        let notes = self.parts.iter().filter_map(|p| match p {
            MessagePart::RuntimeNote { text } => Some(text.as_str()),
            _ => None,
        });
        if retrieved
            .clone()
            .chain(directives.clone())
            .chain(notes.clone())
            .next()
            .is_none()
        {
            return std::borrow::Cow::Borrowed(self.content.as_str());
        }
        let mut out = String::new();
        let pieces = retrieved
            .chain(std::iter::once(self.content.as_str()))
            .chain(directives)
            .chain(notes);
        for piece in pieces {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(piece);
        }
        std::borrow::Cow::Owned(out)
    }

    /// Whether the message has parts and every one is a [`MessagePart::RuntimeNote`]. Repair
    /// treats such a message as empty once its tool results are gone.
    #[must_use]
    pub fn has_only_runtime_notes(&self) -> bool {
        !self.parts.is_empty()
            && self
                .parts
                .iter()
                .all(|p| matches!(p, MessagePart::RuntimeNote { .. }))
    }

    /// Append an agent-authored [`MessagePart::RuntimeNote`] to this message.
    ///
    /// Only a `User` message carrying at least one `ToolResult` part accepts a note, and only
    /// one note per message. `content` is untouched, so `content == flatten(parts)` still holds.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_llm::provider::{Message, MessagePart, NoteAppend, Role};
    ///
    /// let mut msg = Message::from_parts(
    ///     Role::User,
    ///     vec![MessagePart::ToolResult {
    ///         tool_use_id: "t1".into(),
    ///         content: "ok".into(),
    ///         is_error: false,
    ///     }],
    /// );
    /// assert_eq!(msg.append_runtime_note("a".into()), NoteAppend::Appended);
    /// assert_eq!(msg.append_runtime_note("b".into()), NoteAppend::AlreadyPresent);
    ///
    /// let mut plain = Message::from_legacy(Role::User, "hi");
    /// assert_eq!(plain.append_runtime_note("a".into()), NoteAppend::NotAToolResultMessage);
    /// ```
    pub fn append_runtime_note(&mut self, text: String) -> NoteAppend {
        let has_tool_result = self
            .parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolResult { .. }));
        if self.role != Role::User || !has_tool_result {
            return NoteAppend::NotAToolResultMessage;
        }
        if self
            .parts
            .iter()
            .any(|p| matches!(p, MessagePart::RuntimeNote { .. }))
        {
            return NoteAppend::AlreadyPresent;
        }
        self.parts.push(MessagePart::RuntimeNote { text });
        NoteAppend::Appended
    }

    /// Re-synchronize `content` from `parts` after in-place mutation.
    pub fn rebuild_content(&mut self) {
        if !self.parts.is_empty() {
            self.content = Self::flatten_parts(&self.parts);
        }
    }

    pub(crate) fn flatten_parts(parts: &[MessagePart]) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for part in parts {
            match part {
                MessagePart::Text { text }
                | MessagePart::Recall { text }
                | MessagePart::CodeContext { text }
                | MessagePart::Summary { text }
                | MessagePart::CrossSession { text } => out.push_str(text),
                MessagePart::ToolOutput {
                    tool_name,
                    body,
                    compacted_at,
                } => {
                    if compacted_at.is_some() {
                        if body.is_empty() {
                            let _ = write!(out, "[tool output: {tool_name}] (pruned)");
                        } else {
                            let _ = write!(out, "[tool output: {tool_name}] {body}");
                        }
                    } else {
                        let _ = write!(out, "[tool output: {tool_name}]\n```\n{body}\n```");
                    }
                }
                MessagePart::ToolUse { id, name, .. } => {
                    let _ = write!(out, "[tool_use: {name}({id})]");
                }
                MessagePart::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    let _ = write!(out, "[tool_result: {tool_use_id}]\n{content}");
                }
                MessagePart::Image(img) => {
                    let _ = write!(out, "[image: {}, {} bytes]", img.mime_type, img.data.len());
                }
                // Thinking and compaction blocks are internal API metadata — not rendered in text.
                // Runtime notes are request-only and must never leak into `content`.
                MessagePart::ThinkingBlock { .. }
                | MessagePart::RedactedThinkingBlock { .. }
                | MessagePart::Compaction { .. }
                | MessagePart::RuntimeNote { .. }
                | MessagePart::TurnContext(_) => {}
            }
        }
        out
    }
}

/// Core abstraction for all LLM inference backends.
///
/// Every backend — `Ollama`, `Claude`, `OpenAI`, `Gemini`, `Candle` — implements this trait.
/// The [`crate::any::AnyProvider`] enum erases the concrete type so callers can
/// hold any backend behind a single type, and [`crate::router::RouterProvider`]
/// implements this trait to multiplex across multiple backends.
///
/// # Object safety
///
/// This trait is **not** object-safe: 6 methods return `impl Future + Send` (RPIT),
/// and [`chat_typed`](Self::chat_typed) carries a generic type parameter `T`.
/// The `where Self: Sized` bound on `chat_typed` is a mitigation — it excludes that
/// method from the vtable — but the RPIT methods remain and prevent `dyn LlmProvider`.
/// Attempting `Box<dyn LlmProvider>` will produce a compile error.
///
/// For dynamic dispatch, use [`Arc<dyn LlmProviderDyn>`](crate::provider_dyn::LlmProviderDyn)
/// instead — a blanket impl wires every `LlmProvider` implementor automatically.
/// See the [`provider_dyn`](crate::provider_dyn) module for details.
///
/// # Required methods
///
/// Implementors must provide: [`chat`](Self::chat), [`chat_stream`](Self::chat_stream),
/// [`supports_streaming`](Self::supports_streaming), [`embed`](Self::embed),
/// [`supports_embeddings`](Self::supports_embeddings), and [`name`](Self::name).
///
/// # Optional methods
///
/// All other methods have default implementations that are safe to accept:
/// - [`context_window`](Self::context_window) — returns `None`
/// - [`embed_batch`](Self::embed_batch) — sequential fallback via [`embed`](Self::embed)
/// - [`chat_with_tools`](Self::chat_with_tools) — falls back to [`chat`](Self::chat)
/// - [`chat_typed`](Self::chat_typed) — schema-prompt injection + retry
///   (requires `Self: Sized`; use [`chat_typed_dyn`](crate::provider_dyn::chat_typed_dyn)
///   for trait objects)
/// - [`supports_vision`](Self::supports_vision) — returns `false`
/// - [`supports_tool_use`](Self::supports_tool_use) — returns `false`, matching the
///   [`chat_with_tools`](Self::chat_with_tools) default fallback which silently drops
///   tool definitions; providers must override both together to opt into tool use
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_llm::provider::{LlmProvider, Message, Role, ChatStream};
/// use zeph_llm::LlmError;
///
/// struct EchoProvider;
///
/// impl LlmProvider for EchoProvider {
///     async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
///         Ok(messages.last().map(|m| m.content.clone()).unwrap_or_default())
///     }
///
///     async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
///         use zeph_llm::provider::StreamChunk;
///         let text = self.chat(messages).await?;
///         Ok(Box::pin(tokio_stream::once(Ok(StreamChunk::Content(text)))))
///     }
///
///     fn supports_streaming(&self) -> bool { true }
///
///     async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
///         Err(LlmError::EmbedUnsupported { provider: "echo".into() })
///     }
///
///     fn supports_embeddings(&self) -> bool { false }
///
///     fn name(&self) -> &str { "echo" }
/// }
/// ```
pub trait LlmProvider: Send + Sync {
    /// Report the model's context window size in tokens.
    ///
    /// Returns `None` if unknown. Used for auto-budget calculation.
    fn context_window(&self) -> Option<usize> {
        None
    }

    /// Send messages to the LLM and return the assistant response.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider fails to communicate or the response is invalid.
    fn chat(&self, messages: &[Message]) -> impl Future<Output = Result<String, LlmError>> + Send;

    /// Send messages and return a stream of response chunks.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider fails to communicate or the response is invalid.
    fn chat_stream(
        &self,
        messages: &[Message],
    ) -> impl Future<Output = Result<ChatStream, LlmError>> + Send;

    /// Whether this provider supports native streaming.
    fn supports_streaming(&self) -> bool;

    /// Generate an embedding vector from text.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider does not support embeddings or the request fails.
    fn embed(&self, text: &str) -> impl Future<Output = Result<Vec<f32>, LlmError>> + Send;

    /// Embed multiple texts in a single API call.
    ///
    /// Default implementation calls [`embed`][Self::embed] sequentially for each input.
    /// Providers with native batch APIs should override this.
    ///
    /// # Errors
    ///
    /// Returns an error if any embedding fails. On native batch backends the entire batch
    /// fails atomically; on the sequential fallback the first error aborts.
    fn embed_batch(
        &self,
        texts: &[&str],
    ) -> impl Future<Output = Result<Vec<Vec<f32>>, LlmError>> + Send {
        let owned = owned_strs(texts);
        async move {
            let mut results = Vec::with_capacity(owned.len());
            for text in &owned {
                results.push(self.embed(text).await?);
            }
            Ok(results)
        }
    }

    /// Whether this provider supports embedding generation.
    fn supports_embeddings(&self) -> bool;

    /// Provider name for logging and identification.
    fn name(&self) -> &str;

    /// Model identifier string (e.g. `gpt-4o-mini`, `claude-sonnet-5`).
    /// Used by cost-estimation heuristics. Returns `""` when not applicable.
    #[allow(clippy::unnecessary_literal_bound)]
    fn model_identifier(&self) -> &str {
        ""
    }

    /// Model identifier that actually served the most recent dispatch.
    ///
    /// For a concrete single-model provider (Claude, `OpenAI`, Candle, ...) this is
    /// identical to [`model_identifier`](Self::model_identifier) — the default
    /// implementation simply forwards to it. Routing providers (`Router`,
    /// `TriageRouter`) override it to resolve the sub-provider that served the last
    /// call, since their own `model_identifier()` returns a stable routing-policy
    /// label (e.g. `"router"`) rather than a real model id. Callers that need to
    /// reason about the model that produced a specific response (for example,
    /// `is_reasoning_model` detection) should call this method instead of
    /// `model_identifier()`.
    fn effective_model_identifier(&self) -> &str {
        self.model_identifier()
    }

    /// Whether this provider supports image input (vision).
    fn supports_vision(&self) -> bool {
        false
    }

    /// Whether this provider supports native `tool_use` / function calling.
    ///
    /// Defaults to `false` because [`chat_with_tools`](Self::chat_with_tools) defaults
    /// to falling back on [`chat`](Self::chat), which silently discards tool
    /// definitions. Providers implementing real tool calling must override both
    /// this method and `chat_with_tools` together.
    fn supports_tool_use(&self) -> bool {
        false
    }

    /// Send messages with tool definitions, returning a structured response.
    ///
    /// Default: falls back to `chat()` and wraps the result in `ChatResponse::Text`.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider fails to communicate or the response is invalid.
    fn chat_with_tools(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
    ) -> impl std::future::Future<Output = Result<ChatResponse, LlmError>> + Send {
        let msgs = messages.to_vec();
        async move { Ok(ChatResponse::text(self.chat(&msgs).await?)) }
    }

    /// Return the cache usage from the last API call, if available.
    /// Returns `(cache_creation_tokens, cache_read_tokens)`.
    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        None
    }

    /// Return token counts from the last API call, if available.
    /// Returns `(input_tokens, output_tokens)`.
    fn last_usage(&self) -> Option<(u64, u64)> {
        None
    }

    /// Whether this provider's thinking blocks are bound to the conversation prefix.
    ///
    /// Gates [`strip_thinking_after_edit`]. Routers report [`ThinkingBinding::PrefixBound`] when
    /// any member is.
    fn thinking_binding(&self) -> ThinkingBinding {
        ThinkingBinding::Unbound
    }

    /// Whether this provider reuses a cached request prefix across calls.
    ///
    /// Routers report [`PrefixCache::Prefix`] when any member does; wrappers delegate.
    fn prefix_cache(&self) -> PrefixCache {
        PrefixCache::None
    }

    /// Usage billed outside the top-level request counters during the last call (for example
    /// server-side compaction iterations). `None` when the provider reports none.
    fn last_auxiliary_usage(&self) -> Option<AuxiliaryUsage> {
        None
    }

    /// Return reasoning tokens from the last API call, if the provider reports them.
    ///
    /// Reasoning tokens are a **subset** of completion tokens (`OpenAI` o-series only).
    /// Returns `None` for providers that do not expose reasoning token counts.
    fn last_reasoning_tokens(&self) -> Option<u64> {
        None
    }

    /// Return the time-to-first-byte (milliseconds) of the last API call, if available.
    ///
    /// This trait method always returns a TTFB proxy (measured around the HTTP request send,
    /// `retry::send_with_retry` or the equivalent per-attempt retry loop for providers that
    /// don't use that shared helper) — it never reflects true time-to-first-token, even for
    /// calls that streamed. `None` for providers with no HTTP round-trip (Candle) or that
    /// have not made a call yet.
    ///
    /// The one production streaming path (speculative decoding) captures true
    /// time-to-first-token separately, at its own stream-consumption point, and injects it
    /// alongside (overriding) this method's TTFB value when building a `usage_records` row —
    /// see `zeph_core::agent::speculative::stream_drainer::SpeculativeStreamDrainer::drive`.
    /// This trait method itself is not aware of that override; callers that need the true
    /// streaming value must go through that call site, not this one.
    fn last_ttft_ms(&self) -> Option<u64> {
        None
    }

    /// Send messages and return the assistant response together with per-call extras.
    ///
    /// Default implementation calls [`chat`][Self::chat] and returns [`ChatExtras::default()`],
    /// keeping every existing implementor source-compatible at zero cost.
    ///
    /// Providers that support logprobs (`OpenAI`, `Compatible`, `Ollama`) override this to
    /// populate [`ChatExtras::entropy`] with the mean negative log-probability.
    ///
    /// `CoE` is the only caller of this method; the canonical entry point for the agent
    /// loop remains [`chat`][Self::chat].
    ///
    /// # Errors
    ///
    /// Same as [`chat`][Self::chat].
    fn chat_with_extras(
        &self,
        messages: &[Message],
    ) -> impl Future<Output = Result<(String, ChatExtras), LlmError>> + Send {
        let msgs = messages.to_vec();
        async move { Ok((self.chat(&msgs).await?, ChatExtras::default())) }
    }

    /// Return the request payload that will be sent to the provider, for debug dumps.
    ///
    /// Implementations should mirror the provider's request body as closely as practical.
    #[must_use]
    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        _stream: bool,
    ) -> serde_json::Value {
        default_debug_request_json(messages, tools)
    }

    /// Return the list of model identifiers this provider can serve.
    /// Default: empty (provider does not advertise models).
    fn list_models(&self) -> Vec<String> {
        vec![]
    }

    /// Whether this provider supports native structured output.
    fn supports_structured_output(&self) -> bool {
        false
    }

    /// Send messages and parse the response into a typed value `T`.
    ///
    /// Default implementation injects JSON schema into the system prompt and retries once
    /// on parse failure. Providers with native structured output should override this.
    ///
    /// # Object safety
    ///
    /// This method requires `Self: Sized` and is therefore unavailable on trait objects.
    /// Use [`chat_typed_dyn`](crate::provider_dyn::chat_typed_dyn) when working with
    /// `Arc<dyn LlmProviderDyn>`.
    ///
    /// The returned future is `Send`, so generic callers can run it on a multi-threaded runtime
    /// and [`LlmProviderDyn::chat_typed_boxed`](crate::provider_dyn::LlmProviderDyn::chat_typed_boxed)
    /// can box it.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::StructuredParse`] when the response cannot be parsed as `T` after
    /// one retry; propagates any underlying provider error.
    fn chat_typed<T>(
        &self,
        messages: &[Message],
    ) -> impl Future<Output = Result<T, LlmError>> + Send
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema + Send + 'static,
        Self: Sized,
    {
        async move {
            let (_, schema_json) = cached_schema::<T>()?;
            let type_name = short_type_name::<T>();

            let mut augmented = messages.to_vec();
            let instruction = format!(
                "Respond with a valid JSON object matching this schema. \
                 Output ONLY the JSON, no markdown fences or extra text.\n\n\
                 Type: {type_name}\nSchema:\n```json\n{schema_json}\n```"
            );
            augmented.insert(0, Message::from_legacy(Role::System, instruction));

            let raw = self.chat(&augmented).await?;
            let cleaned = strip_json_fences(&raw);
            match serde_json::from_str::<T>(cleaned) {
                Ok(val) => Ok(val),
                Err(first_err) => {
                    augmented.push(Message::from_legacy(Role::Assistant, &raw));
                    augmented.push(Message::from_legacy(
                        Role::User,
                        format!(
                            "Your response was not valid JSON. Error: {first_err}. \
                             Please output ONLY valid JSON matching the schema."
                        ),
                    ));
                    let retry_raw = self.chat(&augmented).await?;
                    let retry_cleaned = strip_json_fences(&retry_raw);
                    serde_json::from_str::<T>(retry_cleaned).map_err(|e| {
                        LlmError::StructuredParse(format!("parse failed after retry: {e}"))
                    })
                }
            }
        }
    }
}

/// Strip markdown code fences from LLM output. Only handles outer fences;
/// JSON containing trailing triple backticks in string values may be
/// incorrectly trimmed (acceptable for MVP — see review R2).
fn strip_json_fences(s: &str) -> &str {
    s.trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use tokio_stream::StreamExt;

    use super::*;

    struct StubProvider {
        response: String,
    }

    impl LlmProvider for StubProvider {
        fn chat(
            &self,
            _messages: &[Message],
        ) -> impl Future<Output = Result<String, LlmError>> + Send {
            std::future::ready(Ok(self.response.clone()))
        }

        async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
            let response = self.chat(messages).await?;
            Ok(Box::pin(tokio_stream::once(Ok(StreamChunk::Content(
                response,
            )))))
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        fn embed(&self, _text: &str) -> impl Future<Output = Result<Vec<f32>, LlmError>> + Send {
            std::future::ready(Ok(vec![0.1, 0.2, 0.3]))
        }

        fn supports_embeddings(&self) -> bool {
            false
        }

        fn name(&self) -> &'static str {
            "stub"
        }
    }

    #[test]
    fn test_image_data_debug_redacts_bytes() {
        let img = ImageData {
            data: vec![0xAB, 0xCD, 0xEF],
            mime_type: "image/png".to_owned(),
        };
        let debug = format!("{img:?}");
        assert_eq!(debug, "[image: image/png, 3 bytes]");
        assert!(!debug.contains("171") && !debug.contains("205") && !debug.contains("239"));
    }

    #[test]
    fn context_window_default_returns_none() {
        let provider = StubProvider {
            response: String::new(),
        };
        assert!(provider.context_window().is_none());
    }

    #[test]
    fn supports_streaming_default_returns_false() {
        let provider = StubProvider {
            response: String::new(),
        };
        assert!(!provider.supports_streaming());
    }

    #[test]
    fn supports_tool_use_default_returns_false() {
        // StubProvider overrides neither `supports_tool_use` nor `chat_with_tools`,
        // mirroring CandleProvider (crates/zeph-llm/src/candle_provider/mod.rs). The
        // default must report `false` so callers gating on this method skip providers
        // that would otherwise silently drop tool definitions via the `chat_with_tools`
        // fallback (issue #5687).
        let provider = StubProvider {
            response: String::new(),
        };
        assert!(!provider.supports_tool_use());
    }

    #[tokio::test]
    async fn chat_stream_default_yields_single_chunk() {
        let provider = StubProvider {
            response: "hello world".into(),
        };
        let messages = vec![Message {
            role: Role::User,
            content: "test".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let mut stream = provider.chat_stream(&messages).await.unwrap();
        let chunk = stream.next().await.unwrap().unwrap();
        assert_matches!(chunk, StreamChunk::Content(s) if s == "hello world");
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn chat_stream_default_propagates_chat_error() {
        struct FailProvider;

        impl LlmProvider for FailProvider {
            fn chat(
                &self,
                _messages: &[Message],
            ) -> impl Future<Output = Result<String, LlmError>> + Send {
                std::future::ready(Err(LlmError::Unavailable))
            }

            async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
                let response = self.chat(messages).await?;
                Ok(Box::pin(tokio_stream::once(Ok(StreamChunk::Content(
                    response,
                )))))
            }

            fn supports_streaming(&self) -> bool {
                false
            }

            fn embed(
                &self,
                _text: &str,
            ) -> impl Future<Output = Result<Vec<f32>, LlmError>> + Send {
                std::future::ready(Err(LlmError::Unavailable))
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
            parts: vec![],
            metadata: MessageMetadata::default(),
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
            fn chat(
                &self,
                _messages: &[Message],
            ) -> impl Future<Output = Result<String, LlmError>> + Send {
                std::future::ready(Err(LlmError::Unavailable))
            }

            async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
                let response = self.chat(messages).await?;
                Ok(Box::pin(tokio_stream::once(Ok(StreamChunk::Content(
                    response,
                )))))
            }

            fn supports_streaming(&self) -> bool {
                false
            }

            fn embed(
                &self,
                _text: &str,
            ) -> impl Future<Output = Result<Vec<f32>, LlmError>> + Send {
                std::future::ready(Err(LlmError::EmbedUnsupported {
                    provider: "fail".into(),
                }))
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
                .contains("embedding not supported")
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
            parts: vec![],
            metadata: MessageMetadata::default(),
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
            parts: vec![],
            metadata: MessageMetadata::default(),
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
            parts: vec![],
            metadata: MessageMetadata::default(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"hello\""));
    }

    #[test]
    fn message_part_serde_round_trip() {
        let parts = vec![
            MessagePart::Text {
                text: "hello".into(),
            },
            MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: "output".into(),
                compacted_at: None,
            },
            MessagePart::Recall {
                text: "recall".into(),
            },
            MessagePart::CodeContext {
                text: "code".into(),
            },
            MessagePart::Summary {
                text: "summary".into(),
            },
        ];
        let json = serde_json::to_string(&parts).unwrap();
        let deserialized: Vec<MessagePart> = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.len(), 5);
    }

    #[test]
    fn from_legacy_creates_empty_parts() {
        let msg = Message::from_legacy(Role::User, "hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, "hello");
        assert!(msg.parts.is_empty());
        assert_eq!(msg.to_llm_content(), "hello");
    }

    #[test]
    fn from_parts_flattens_content() {
        let msg = Message::from_parts(
            Role::System,
            vec![MessagePart::Recall {
                text: "recalled data".into(),
            }],
        );
        assert_eq!(msg.content, "recalled data");
        assert_eq!(msg.to_llm_content(), "recalled data");
        assert_eq!(msg.parts.len(), 1);
    }

    #[test]
    fn from_parts_tool_output_format() {
        let msg = Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: "hello world".into(),
                compacted_at: None,
            }],
        );
        assert!(msg.content.contains("[tool output: bash]"));
        assert!(msg.content.contains("hello world"));
    }

    #[test]
    fn message_deserializes_without_parts() {
        let json = r#"{"role":"user","content":"hello"}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert_eq!(msg.content, "hello");
        assert!(msg.parts.is_empty());
    }

    #[test]
    fn flatten_skips_compacted_tool_output_empty_body() {
        // When compacted_at is set and body is empty, renders "(pruned)".
        let msg = Message::from_parts(
            Role::User,
            vec![
                MessagePart::Text {
                    text: "prefix ".into(),
                },
                MessagePart::ToolOutput {
                    tool_name: "bash".into(),
                    body: String::new(),
                    compacted_at: Some(1234),
                },
                MessagePart::Text {
                    text: " suffix".into(),
                },
            ],
        );
        assert!(msg.content.contains("(pruned)"));
        assert!(msg.content.contains("prefix "));
        assert!(msg.content.contains(" suffix"));
    }

    #[test]
    fn flatten_compacted_tool_output_with_reference_renders_body() {
        // When compacted_at is set and body contains a reference notice, renders the body.
        let ref_notice = "[tool output pruned; full content at /tmp/overflow/big.txt]";
        let msg = Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: ref_notice.into(),
                compacted_at: Some(1234),
            }],
        );
        assert!(msg.content.contains(ref_notice));
        assert!(!msg.content.contains("(pruned)"));
    }

    #[test]
    fn rebuild_content_syncs_after_mutation() {
        let mut msg = Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: "original".into(),
                compacted_at: None,
            }],
        );
        assert!(msg.content.contains("original"));

        if let MessagePart::ToolOutput {
            ref mut compacted_at,
            ref mut body,
            ..
        } = msg.parts[0]
        {
            *compacted_at = Some(999);
            body.clear(); // simulate pruning: body cleared, no overflow notice
        }
        msg.rebuild_content();

        assert!(msg.content.contains("(pruned)"));
        assert!(!msg.content.contains("original"));
    }

    #[test]
    fn message_part_tool_use_serde_round_trip() {
        let part = MessagePart::ToolUse {
            id: "toolu_123".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        };
        let json = serde_json::to_string(&part).unwrap();
        let deserialized: MessagePart = serde_json::from_str(&json).unwrap();
        if let MessagePart::ToolUse { id, name, input } = deserialized {
            assert_eq!(id, "toolu_123");
            assert_eq!(name, "bash");
            assert_eq!(input["command"], "ls");
        } else {
            panic!("expected ToolUse");
        }
    }

    #[test]
    fn message_part_tool_result_serde_round_trip() {
        let part = MessagePart::ToolResult {
            tool_use_id: "toolu_123".into(),
            content: "file1.rs\nfile2.rs".into(),
            is_error: false,
        };
        let json = serde_json::to_string(&part).unwrap();
        let deserialized: MessagePart = serde_json::from_str(&json).unwrap();
        if let MessagePart::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = deserialized
        {
            assert_eq!(tool_use_id, "toolu_123");
            assert_eq!(content, "file1.rs\nfile2.rs");
            assert!(!is_error);
        } else {
            panic!("expected ToolResult");
        }
    }

    #[test]
    fn message_part_tool_result_is_error_default() {
        let json = r#"{"kind":"tool_result","tool_use_id":"id","content":"err"}"#;
        let part: MessagePart = serde_json::from_str(json).unwrap();
        if let MessagePart::ToolResult { is_error, .. } = part {
            assert!(!is_error);
        } else {
            panic!("expected ToolResult");
        }
    }

    #[test]
    fn chat_response_construction() {
        let text = ChatResponse::text("hello");
        assert_matches!(text, ChatResponse::Text { text: s, .. } if s == "hello");

        let tool_use = ChatResponse::tool_use(
            Some("I'll run that".into()),
            vec![ToolUseRequest {
                id: "1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            }],
        );
        assert_matches!(tool_use, ChatResponse::ToolUse { .. });
    }

    #[test]
    fn flatten_parts_tool_use() {
        let msg = Message::from_parts(
            Role::Assistant,
            vec![MessagePart::ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }],
        );
        assert!(msg.content.contains("[tool_use: bash(t1)]"));
    }

    #[test]
    fn flatten_parts_tool_result() {
        let msg = Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "t1".into(),
                content: "output here".into(),
                is_error: false,
            }],
        );
        assert!(msg.content.contains("[tool_result: t1]"));
        assert!(msg.content.contains("output here"));
    }

    #[test]
    fn tool_definition_serde_round_trip() {
        let def = ToolDefinition {
            name: "bash".into(),
            description: "Execute a shell command".into(),
            parameters: serde_json::json!({"type": "object"}),
            output_schema: None,
        };
        let json = serde_json::to_string(&def).unwrap();
        let deserialized: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "bash");
        assert_eq!(deserialized.description, "Execute a shell command");
    }

    #[tokio::test]
    async fn chat_with_tools_default_delegates_to_chat() {
        let provider = StubProvider {
            response: "hello".into(),
        };
        let messages = vec![Message::from_legacy(Role::User, "test")];
        let result = provider.chat_with_tools(&messages, &[]).await.unwrap();
        assert_matches!(result, ChatResponse::Text { text: s, .. } if s == "hello");
    }

    #[test]
    fn tool_output_compacted_at_serde_default() {
        let json = r#"{"kind":"tool_output","tool_name":"bash","body":"out"}"#;
        let part: MessagePart = serde_json::from_str(json).unwrap();
        if let MessagePart::ToolOutput { compacted_at, .. } = part {
            assert!(compacted_at.is_none());
        } else {
            panic!("expected ToolOutput");
        }
    }

    // --- M27: strip_json_fences tests ---

    #[test]
    fn strip_json_fences_plain_json() {
        assert_eq!(strip_json_fences(r#"{"a": 1}"#), r#"{"a": 1}"#);
    }

    #[test]
    fn strip_json_fences_with_json_fence() {
        assert_eq!(strip_json_fences("```json\n{\"a\": 1}\n```"), r#"{"a": 1}"#);
    }

    #[test]
    fn strip_json_fences_with_plain_fence() {
        assert_eq!(strip_json_fences("```\n{\"a\": 1}\n```"), r#"{"a": 1}"#);
    }

    #[test]
    fn strip_json_fences_whitespace() {
        assert_eq!(strip_json_fences("  \n  "), "");
    }

    #[test]
    fn strip_json_fences_empty() {
        assert_eq!(strip_json_fences(""), "");
    }

    #[test]
    fn strip_json_fences_outer_whitespace() {
        assert_eq!(
            strip_json_fences("  ```json\n{\"a\": 1}\n```  "),
            r#"{"a": 1}"#
        );
    }

    #[test]
    fn strip_json_fences_only_opening_fence() {
        assert_eq!(strip_json_fences("```json\n{\"a\": 1}"), r#"{"a": 1}"#);
    }

    // --- M27: chat_typed tests ---

    #[derive(Debug, serde::Deserialize, schemars::JsonSchema, PartialEq)]
    struct TestOutput {
        value: String,
    }

    struct SequentialStub {
        responses: std::sync::Mutex<Vec<Result<String, LlmError>>>,
    }

    impl SequentialStub {
        fn new(responses: Vec<Result<String, LlmError>>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
            }
        }
    }

    impl LlmProvider for SequentialStub {
        #[allow(clippy::unused_async_trait_impl)]
        async fn chat(&self, _messages: &[Message]) -> Result<String, LlmError> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Err(LlmError::Other("no more responses".into()))
            } else {
                responses.remove(0)
            }
        }

        async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
            let response = self.chat(messages).await?;
            Ok(Box::pin(tokio_stream::once(Ok(StreamChunk::Content(
                response,
            )))))
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        fn embed(&self, _text: &str) -> impl Future<Output = Result<Vec<f32>, LlmError>> + Send {
            std::future::ready(Err(LlmError::EmbedUnsupported {
                provider: "sequential-stub".into(),
            }))
        }

        fn supports_embeddings(&self) -> bool {
            false
        }

        fn name(&self) -> &'static str {
            "sequential-stub"
        }
    }

    #[tokio::test]
    async fn chat_typed_happy_path() {
        let provider = StubProvider {
            response: r#"{"value": "hello"}"#.into(),
        };
        let messages = vec![Message::from_legacy(Role::User, "test")];
        let result: TestOutput = provider.chat_typed(&messages).await.unwrap();
        assert_eq!(
            result,
            TestOutput {
                value: "hello".into()
            }
        );
    }

    #[tokio::test]
    async fn chat_typed_retry_succeeds() {
        let provider = SequentialStub::new(vec![
            Ok("not valid json".into()),
            Ok(r#"{"value": "ok"}"#.into()),
        ]);
        let messages = vec![Message::from_legacy(Role::User, "test")];
        let result: TestOutput = provider.chat_typed(&messages).await.unwrap();
        assert_eq!(result, TestOutput { value: "ok".into() });
    }

    #[tokio::test]
    async fn chat_typed_both_fail() {
        let provider = SequentialStub::new(vec![Ok("bad json".into()), Ok("still bad".into())]);
        let messages = vec![Message::from_legacy(Role::User, "test")];
        let result = provider.chat_typed::<TestOutput>(&messages).await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("parse failed after retry"));
    }

    #[tokio::test]
    async fn chat_typed_chat_error_propagates() {
        let provider = SequentialStub::new(vec![Err(LlmError::Unavailable)]);
        let messages = vec![Message::from_legacy(Role::User, "test")];
        let result = provider.chat_typed::<TestOutput>(&messages).await;
        assert_matches!(result, Err(LlmError::Unavailable));
    }

    #[tokio::test]
    async fn chat_typed_strips_fences() {
        let provider = StubProvider {
            response: "```json\n{\"value\": \"fenced\"}\n```".into(),
        };
        let messages = vec![Message::from_legacy(Role::User, "test")];
        let result: TestOutput = provider.chat_typed(&messages).await.unwrap();
        assert_eq!(
            result,
            TestOutput {
                value: "fenced".into()
            }
        );
    }

    #[test]
    fn supports_structured_output_default_false() {
        let provider = StubProvider {
            response: String::new(),
        };
        assert!(!provider.supports_structured_output());
    }

    #[test]
    fn structured_parse_error_display() {
        let err = LlmError::StructuredParse("test error".into());
        assert_eq!(
            err.to_string(),
            "structured output parse failed: test error"
        );
    }

    #[test]
    fn message_part_image_roundtrip_json() {
        let part = MessagePart::Image(Box::new(ImageData {
            data: vec![1, 2, 3, 4],
            mime_type: "image/jpeg".into(),
        }));
        let json = serde_json::to_string(&part).unwrap();
        let decoded: MessagePart = serde_json::from_str(&json).unwrap();
        match decoded {
            MessagePart::Image(img) => {
                assert_eq!(img.data, vec![1, 2, 3, 4]);
                assert_eq!(img.mime_type, "image/jpeg");
            }
            _ => panic!("expected Image variant"),
        }
    }

    #[test]
    fn flatten_parts_includes_image_placeholder() {
        let msg = Message::from_parts(
            Role::User,
            vec![
                MessagePart::Text {
                    text: "see this".into(),
                },
                MessagePart::Image(Box::new(ImageData {
                    data: vec![0u8; 100],
                    mime_type: "image/png".into(),
                })),
            ],
        );
        let content = msg.to_llm_content();
        assert!(content.contains("see this"));
        assert!(content.contains("[image: image/png"));
    }

    #[test]
    fn supports_vision_default_false() {
        let provider = StubProvider {
            response: String::new(),
        };
        assert!(!provider.supports_vision());
    }

    #[test]
    fn message_metadata_default_both_visible() {
        let m = MessageMetadata::default();
        assert!(m.visibility.is_agent_visible());
        assert!(m.visibility.is_user_visible());
        assert_eq!(m.visibility, MessageVisibility::Both);
        assert!(m.compacted_at.is_none());
    }

    #[test]
    fn message_metadata_agent_only() {
        let m = MessageMetadata::agent_only();
        assert!(m.visibility.is_agent_visible());
        assert!(!m.visibility.is_user_visible());
        assert_eq!(m.visibility, MessageVisibility::AgentOnly);
    }

    #[test]
    fn message_metadata_user_only() {
        let m = MessageMetadata::user_only();
        assert!(!m.visibility.is_agent_visible());
        assert!(m.visibility.is_user_visible());
        assert_eq!(m.visibility, MessageVisibility::UserOnly);
    }

    #[test]
    fn message_metadata_serde_default() {
        let json = r#"{"role":"user","content":"hello"}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert!(msg.metadata.visibility.is_agent_visible());
        assert!(msg.metadata.visibility.is_user_visible());
    }

    #[test]
    fn message_metadata_round_trip() {
        let msg = Message {
            role: Role::User,
            content: "test".into(),
            parts: vec![],
            metadata: MessageMetadata::agent_only(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        assert!(decoded.metadata.visibility.is_agent_visible());
        assert!(!decoded.metadata.visibility.is_user_visible());
        assert_eq!(decoded.metadata.visibility, MessageVisibility::AgentOnly);
    }

    #[test]
    fn message_part_compaction_round_trip() {
        let part = MessagePart::Compaction {
            summary: "Context was summarized.".to_owned(),
        };
        let json = serde_json::to_string(&part).unwrap();
        let decoded: MessagePart = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, MessagePart::Compaction { summary } if summary == "Context was summarized.")
        );
    }

    #[test]
    fn flatten_parts_compaction_contributes_no_text() {
        // MessagePart::Compaction must not appear in the flattened content string
        // (it's metadata-only; the summary is stored on the Message separately).
        let parts = vec![
            MessagePart::Text {
                text: "Hello".to_owned(),
            },
            MessagePart::Compaction {
                summary: "Summary".to_owned(),
            },
        ];
        let msg = Message::from_parts(Role::Assistant, parts);
        // Only the Text part should appear in content.
        assert_eq!(msg.content.trim(), "Hello");
    }

    #[test]
    fn stream_chunk_compaction_variant() {
        let chunk = StreamChunk::Compaction("A summary".to_owned());
        assert_matches!(chunk, StreamChunk::Compaction(s) if s == "A summary");
    }

    #[test]
    fn short_type_name_extracts_last_segment() {
        struct MyOutput;
        assert_eq!(short_type_name::<MyOutput>(), "MyOutput");
    }

    #[test]
    fn short_type_name_primitive_returns_full_name() {
        // Primitives have no "::" in their type_name — rsplit returns the full name.
        assert_eq!(short_type_name::<u32>(), "u32");
        assert_eq!(short_type_name::<bool>(), "bool");
    }

    #[test]
    fn short_type_name_nested_path_returns_last() {
        // Use a type whose path contains "::" segments.
        assert_eq!(
            short_type_name::<std::collections::HashMap<u32, u32>>(),
            "HashMap<u32, u32>"
        );
    }

    // Regression test for #2257: `MessagePart::Summary` must serialize to the
    // internally-tagged format `{"kind":"summary","text":"..."}` and round-trip correctly.
    #[test]
    fn summary_roundtrip() {
        let part = MessagePart::Summary {
            text: "hello".to_string(),
        };
        let json = serde_json::to_string(&part).expect("serialization must not fail");
        assert!(
            json.contains("\"kind\":\"summary\""),
            "must use internally-tagged format, got: {json}"
        );
        assert!(
            !json.contains("\"Summary\""),
            "must not use externally-tagged format, got: {json}"
        );
        let decoded: MessagePart =
            serde_json::from_str(&json).expect("deserialization must not fail");
        match decoded {
            MessagePart::Summary { text } => assert_eq!(text, "hello"),
            other => panic!("expected MessagePart::Summary, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn embed_batch_default_empty_returns_empty() {
        let provider = StubProvider {
            response: String::new(),
        };
        let result = provider.embed_batch(&[]).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn embed_batch_default_calls_embed_sequentially() {
        let provider = StubProvider {
            response: String::new(),
        };
        let texts = ["hello", "world", "foo"];
        let result = provider.embed_batch(&texts).await.unwrap();
        assert_eq!(result.len(), 3);
        // StubProvider::embed always returns [0.1, 0.2, 0.3]
        for vec in &result {
            assert_eq!(vec, &[0.1_f32, 0.2, 0.3]);
        }
    }

    #[test]
    fn message_visibility_db_roundtrip_both() {
        assert_eq!(MessageVisibility::Both.as_db_str(), "both");
        assert_eq!(
            MessageVisibility::from_db_str("both"),
            MessageVisibility::Both
        );
    }

    #[test]
    fn message_visibility_db_roundtrip_marker() {
        assert_eq!(MessageVisibility::Marker.as_db_str(), "marker");
        assert_eq!(
            MessageVisibility::from_db_str("marker"),
            MessageVisibility::Marker
        );
        assert!(!MessageVisibility::Marker.is_agent_visible());
        assert!(!MessageVisibility::Marker.is_user_visible());
    }

    #[test]
    fn marker_message_is_filtered_from_agent_visible() {
        let mut m = Message::from_legacy(Role::System, "[focus checkpoint: x]");
        m.metadata = MessageMetadata::marker();
        let messages = vec![Message::from_legacy(Role::System, "sys"), m];
        assert_eq!(AgentVisible::new(&messages).len(), 1);
    }

    #[test]
    fn message_visibility_db_roundtrip_agent_only() {
        assert_eq!(MessageVisibility::AgentOnly.as_db_str(), "agent_only");
        assert_eq!(
            MessageVisibility::from_db_str("agent_only"),
            MessageVisibility::AgentOnly
        );
    }

    #[test]
    fn message_visibility_db_roundtrip_user_only() {
        assert_eq!(MessageVisibility::UserOnly.as_db_str(), "user_only");
        assert_eq!(
            MessageVisibility::from_db_str("user_only"),
            MessageVisibility::UserOnly
        );
    }

    #[test]
    fn message_visibility_from_db_str_unknown_defaults_to_both() {
        assert_eq!(
            MessageVisibility::from_db_str("unknown_future_value"),
            MessageVisibility::Both
        );
        assert_eq!(MessageVisibility::from_db_str(""), MessageVisibility::Both);
    }

    fn compaction_msg(summary: &str) -> Message {
        Message::from_parts(
            Role::Assistant,
            vec![MessagePart::Compaction {
                summary: summary.to_owned(),
            }],
        )
    }

    #[test]
    fn context_floor_is_one_without_compaction() {
        let msgs = vec![
            Message::from_legacy(Role::System, "sys"),
            Message::from_legacy(Role::User, "hi"),
        ];
        assert_eq!(context_floor(&msgs), 1);
        assert_eq!(context_floor(&[]), 1);
    }

    #[test]
    fn context_floor_follows_last_compaction() {
        let msgs = vec![
            Message::from_legacy(Role::System, "sys"),
            compaction_msg("old"),
            Message::from_legacy(Role::User, "a"),
            compaction_msg("new"),
            Message::from_legacy(Role::User, "b"),
        ];
        assert_eq!(context_floor(&msgs), 4);
    }

    #[test]
    fn insert_at_context_floor_lands_after_compaction_block() {
        let mut msgs = vec![
            Message::from_legacy(Role::System, "sys"),
            compaction_msg("s"),
            Message::from_legacy(Role::User, "turn"),
        ];
        insert_at_context_floor(&mut msgs, Message::from_legacy(Role::System, "recall"));
        assert_eq!(msgs[1].parts.len(), 1);
        assert_eq!(msgs[2].content, "recall");
        assert_eq!(msgs[3].content, "turn");

        let mut plain = vec![Message::from_legacy(Role::System, "sys")];
        insert_at_context_floor(&mut plain, Message::from_legacy(Role::User, "x"));
        assert_eq!(plain[1].content, "x");
    }

    #[test]
    fn message_origin_round_trips_and_defaults_to_none() {
        let mut md = MessageMetadata::default();
        assert_eq!(md.origin, None);
        let json = serde_json::to_string(&md).unwrap();
        assert!(!json.contains("origin"));
        md.origin = Some(MessageOrigin::TurnInput);
        let back: MessageMetadata =
            serde_json::from_str(&serde_json::to_string(&md).unwrap()).unwrap();
        assert_eq!(back.origin, Some(MessageOrigin::TurnInput));
    }

    #[test]
    fn agent_visible_adjacency_skips_hidden_and_system_messages() {
        let hidden = Message {
            metadata: MessageMetadata::user_only(),
            ..Message::from_legacy(Role::User, "hidden")
        };
        let messages = vec![
            Message::from_legacy(Role::Assistant, "a"),
            hidden,
            Message::from_legacy(Role::System, "sys"),
            Message::from_legacy(Role::User, "b"),
        ];
        let visible = AgentVisible::new(&messages);
        assert_eq!(visible.len(), 3);
        assert_eq!(visible.next_non_system(0).unwrap().content, "b");
        assert_eq!(visible.prev_non_system(2).unwrap().content, "a");
        assert!(visible.next_non_system(2).is_none());
    }

    fn tool_result_message(id: &str) -> Message {
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: id.into(),
                content: "output".into(),
                is_error: false,
            }],
        )
    }

    #[test]
    fn runtime_note_appends_once_and_keeps_content_in_sync() {
        let mut msg = tool_result_message("t1");
        let before = msg.content.clone();

        assert_eq!(
            msg.append_runtime_note("<runtime-note>a</runtime-note>".into()),
            NoteAppend::Appended
        );
        assert_eq!(
            msg.append_runtime_note("<runtime-note>b</runtime-note>".into()),
            NoteAppend::AlreadyPresent
        );

        assert_eq!(msg.content, before);
        assert_eq!(msg.content, Message::flatten_parts(&msg.parts));
        assert_eq!(
            msg.parts
                .iter()
                .filter(|p| matches!(p, MessagePart::RuntimeNote { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn runtime_note_requires_a_user_tool_result_message() {
        let mut plain = Message::from_legacy(Role::User, "hi");
        let mut assistant = Message::from_parts(
            Role::Assistant,
            vec![MessagePart::ToolResult {
                tool_use_id: "t".into(),
                content: "c".into(),
                is_error: false,
            }],
        );
        assert_eq!(
            plain.append_runtime_note("n".into()),
            NoteAppend::NotAToolResultMessage
        );
        assert_eq!(
            assistant.append_runtime_note("n".into()),
            NoteAppend::NotAToolResultMessage
        );
        assert!(plain.parts.is_empty());
    }

    #[test]
    fn runtime_note_is_request_text_only() {
        let mut msg = tool_result_message("t1");
        msg.append_runtime_note("NOTE".into());

        assert!(!msg.content.contains("NOTE"));
        assert!(msg.parts.iter().all(|p| p.as_plain_text().is_none()));
        assert_eq!(
            msg.parts.last().and_then(MessagePart::as_request_text),
            Some("NOTE")
        );
        assert!(msg.to_request_text().ends_with("\nNOTE"));
        assert!(matches!(
            Message::from_legacy(Role::User, "x").to_request_text(),
            std::borrow::Cow::Borrowed("x")
        ));
    }

    fn thinking_part() -> MessagePart {
        MessagePart::ThinkingBlock {
            thinking: "t".into(),
            signature: "s".into(),
        }
    }

    #[test]
    fn strip_thinking_from_keeps_everything_before_the_edit_point() {
        let mut msgs = vec![
            Message::from_parts(Role::Assistant, vec![thinking_part()]),
            Message::from_parts(
                Role::Assistant,
                vec![thinking_part(), MessagePart::Text { text: "x".into() }],
            ),
            Message::from_parts(
                Role::Assistant,
                vec![
                    MessagePart::RedactedThinkingBlock { data: "d".into() },
                    MessagePart::Text { text: "y".into() },
                ],
            ),
        ];
        assert_eq!(strip_thinking_from(&mut msgs, 1), 2);
        assert_eq!(msgs[0].parts.len(), 1, "before the edit point");
        assert_eq!(msgs[1].parts.len(), 1);
        assert_eq!(msgs[2].parts.len(), 1);
        assert_eq!(msgs[1].content, "x");
        assert_eq!(strip_thinking_from(&mut msgs, 1), 0, "idempotent");
        assert_eq!(
            strip_thinking_from(&mut msgs, 99),
            0,
            "out of range is a no-op"
        );
    }

    #[test]
    fn content_builder_preserves_arrival_order_and_merges_adjacent_text() {
        let mut b = AssistantContentBuilder::default();
        b.push_text("a");
        b.push_text("b");
        b.push_thinking(ThinkingBlock::Thinking {
            thinking: "t".into(),
            signature: "s".into(),
        });
        b.push_text("c");
        b.push_tool_use(ToolUseRequest {
            id: "1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        });
        let content = b.build();
        assert!(matches!(
            content.blocks(),
            [
                AssistantBlock::Text(a),
                AssistantBlock::Thinking(_),
                AssistantBlock::Text(c),
                AssistantBlock::ToolUse(_),
            ] if a == "ab" && c == "c"
        ));
        assert_eq!(content.text().as_deref(), Some("abc"));
        assert_eq!(content.tool_calls().count(), 1);
    }

    #[test]
    fn compaction_travels_with_a_tool_use_response_and_is_taken_once() {
        let mut resp = ChatResponse::tool_use(None, vec![]);
        resp.attach_compaction("summary".into());
        assert_eq!(resp.take_compaction().as_deref(), Some("summary"));
        assert_eq!(resp.take_compaction(), None);
        let mut text = ChatResponse::text("t");
        text.attach_compaction("kept".into());
        assert_eq!(text.take_compaction().as_deref(), Some("kept"));
    }

    #[test]
    fn response_strip_thinking_keeps_other_blocks_in_order() {
        let mut resp = ChatResponse::ToolUse {
            content: AssistantContent::new(vec![
                AssistantBlock::Thinking(ThinkingBlock::Redacted { data: "d".into() }),
                AssistantBlock::Text("t".into()),
                AssistantBlock::ToolUse(ToolUseRequest {
                    id: "1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                }),
            ]),
            replay: ThinkingReplay::Intact,
            compaction: None,
        };
        resp.strip_thinking();
        let ChatResponse::ToolUse { content, .. } = resp else {
            panic!("expected ToolUse");
        };
        assert!(matches!(
            content.blocks(),
            [AssistantBlock::Text(_), AssistantBlock::ToolUse(_)]
        ));
    }

    #[test]
    fn strip_for_persistence_drops_notes_images_and_thinking_only() {
        let parts = vec![
            MessagePart::ThinkingBlock {
                thinking: "x".into(),
                signature: "s".into(),
            },
            MessagePart::RedactedThinkingBlock { data: "d".into() },
            MessagePart::Text { text: "t".into() },
            MessagePart::RuntimeNote { text: "n".into() },
            MessagePart::Image(Box::new(ImageData {
                data: vec![1],
                mime_type: "image/png".into(),
            })),
        ];
        let stripped = MessagePart::strip_for_persistence(&parts);
        assert_eq!(stripped.len(), 1);
        assert!(matches!(stripped[0], MessagePart::Text { .. }));
    }

    #[test]
    fn turn_context_is_request_only() {
        let msg = crate::testing::turn_context_message();
        assert_eq!(msg.content, "question", "never part of content");
        assert_eq!(msg.parts[1].as_plain_text(), Some("question"));
        assert_eq!(msg.parts[0].as_plain_text(), None);
        assert_eq!(
            msg.to_request_text(),
            crate::testing::TURN_CONTEXT_REQUEST_TEXT
        );
        let persisted = MessagePart::strip_for_persistence(&msg.parts);
        assert_eq!(persisted.len(), 1);
        assert!(matches!(persisted[0], MessagePart::Text { .. }));
    }

    #[test]
    fn request_metadata_is_never_serialized() {
        let metadata = MessageMetadata {
            request_policy: Some(RequestPolicy {
                thinking: ThinkingScope::Verbatim,
                conversation: ConversationKey(1),
            }),
            ..MessageMetadata::default()
        };
        let json = serde_json::to_string(&metadata).unwrap();
        assert!(!json.contains("request_policy"));
        let back: MessageMetadata = serde_json::from_str(&json).unwrap();
        assert!(back.request_policy.is_none());
    }

    #[test]
    fn prefix_cache_or_never_shortens_a_ttl_to_an_unknown_or_smaller_one() {
        use std::time::Duration;

        let five = PrefixCache::Prefix {
            ttl: Some(Duration::from_mins(5)),
        };
        let hour = PrefixCache::Prefix {
            ttl: Some(Duration::from_hours(1)),
        };
        let unknown = PrefixCache::Prefix { ttl: None };
        assert_eq!(five.or(hour), hour);
        assert_eq!(hour.or(five), hour);
        assert_eq!(five.or(unknown), unknown);
        assert_eq!(unknown.or(five), unknown);
        assert_eq!(PrefixCache::None.or(five), five);
        assert_eq!(five.or(PrefixCache::None), five);
        assert_eq!(unknown.or(unknown), unknown);
    }

    #[test]
    fn has_only_runtime_notes_is_false_for_empty_and_mixed_parts() {
        let mut msg = tool_result_message("t1");
        assert!(!msg.has_only_runtime_notes());
        msg.append_runtime_note("n".into());
        assert!(!msg.has_only_runtime_notes());
        msg.parts
            .retain(|p| matches!(p, MessagePart::RuntimeNote { .. }));
        assert!(msg.has_only_runtime_notes());
        assert!(!Message::from_legacy(Role::User, "x").has_only_runtime_notes());
    }

    #[test]
    fn turn_query_message_fallback_skips_head_injections() {
        let mut injected = Message::from_legacy(Role::User, "[recall] stale");
        injected.metadata.origin = Some(MessageOrigin::HeadInjection);
        let msgs = vec![Message::from_legacy(Role::User, "real question"), injected];
        assert_eq!(turn_query_message(&msgs).unwrap().content, "real question");
    }

    #[test]
    fn turn_query_message_prefers_turn_input_then_plain_user_message() {
        let mut query = Message::from_legacy(Role::User, "the query");
        query.metadata.origin = Some(MessageOrigin::TurnInput);
        let msgs = vec![
            Message::from_legacy(Role::User, "older"),
            query,
            tool_result_message("t1"),
        ];
        assert_eq!(turn_query_message(&msgs).unwrap().content, "the query");

        let no_origin = vec![
            Message::from_legacy(Role::User, "plain question"),
            tool_result_message("t1"),
        ];
        assert_eq!(
            turn_query_message(&no_origin).unwrap().content,
            "plain question"
        );
        assert!(turn_query_message(&[tool_result_message("t1")]).is_none());
    }
}
