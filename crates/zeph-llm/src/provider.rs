// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::future::Future;
use std::pin::Pin;
#[cfg(feature = "schema")]
use std::{
    any::TypeId,
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::error::LlmError;

#[cfg(feature = "schema")]
static SCHEMA_CACHE: LazyLock<Mutex<HashMap<TypeId, (serde_json::Value, String)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Return the JSON schema value and pretty-printed string for type `T`, cached by `TypeId`.
///
/// # Errors
///
/// Returns an error if schema serialization fails.
#[cfg(feature = "schema")]
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

/// A chunk from an LLM streaming response.
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
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, LlmError>> + Send>>;

/// Minimal tool definition for LLM providers.
///
/// Decoupled from `zeph-tools::ToolDef` to avoid cross-crate dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing parameters.
    pub parameters: serde_json::Value,
}

/// Structured tool invocation request from the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseRequest {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// Thinking block returned by Claude when thinking is enabled.
#[derive(Debug, Clone)]
pub enum ThinkingBlock {
    Thinking { thinking: String, signature: String },
    Redacted { data: String },
}

/// Marker injected into `ChatResponse::Text` when the LLM response was cut off by the
/// token limit. Consumers can detect this substring to signal `MaxTokens` stop reason.
pub const MAX_TOKENS_TRUNCATION_MARKER: &str = "max_tokens limit reached";

/// Response from `chat_with_tools()`.
#[derive(Debug, Clone)]
pub enum ChatResponse {
    /// Model produced text output only.
    Text(String),
    /// Model requests one or more tool invocations.
    ToolUse {
        /// Any text the model emitted before/alongside tool calls.
        text: Option<String>,
        tool_calls: Vec<ToolUseRequest>,
        /// Thinking blocks from the model (empty when thinking is disabled).
        /// Must be preserved verbatim in multi-turn requests.
        thinking_blocks: Vec<ThinkingBlock>,
    },
}

/// Boxed future returning an embedding vector.
pub type EmbedFuture = Pin<Box<dyn Future<Output = Result<Vec<f32>, LlmError>> + Send>>;

/// Closure type for embedding text into a vector.
pub type EmbedFn = Box<dyn Fn(&str) -> EmbedFuture + Send + Sync>;

/// Sender for emitting status events (retries, fallbacks) to the UI.
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
#[derive(Debug, Clone, Default)]
pub struct GenerationOverrides {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<usize>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessagePart {
    Text {
        text: String,
    },
    ToolOutput {
        tool_name: String,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted_at: Option<i64>,
    },
    Recall {
        text: String,
    },
    CodeContext {
        text: String,
    },
    Summary {
        text: String,
    },
    CrossSession {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    Image(Box<ImageData>),
    /// Claude thinking block — must be preserved verbatim in multi-turn requests.
    ThinkingBlock {
        thinking: String,
        signature: String,
    },
    /// Claude redacted thinking block — preserved as-is in multi-turn requests.
    RedactedThinkingBlock {
        data: String,
    },
    /// Claude server-side compaction block — must be preserved verbatim in multi-turn requests
    /// so the API can correctly prune prior history on the next turn.
    Compaction {
        summary: String,
    },
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

    /// Return the image data if this part is an `Image` variant, `None` otherwise.
    #[must_use]
    pub fn as_image(&self) -> Option<&ImageData> {
        if let Self::Image(img) = self {
            Some(img)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageData {
    #[serde(with = "serde_bytes_base64")]
    pub data: Vec<u8>,
    pub mime_type: String,
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

/// Per-message visibility flags controlling agent context and user display.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageMetadata {
    pub agent_visible: bool,
    pub user_visible: bool,
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
}

impl Default for MessageMetadata {
    fn default() -> Self {
        Self {
            agent_visible: true,
            user_visible: true,
            compacted_at: None,
            deferred_summary: None,
            focus_pinned: false,
            focus_marker_id: None,
        }
    }
}

impl MessageMetadata {
    /// Message visible only to the agent (e.g. compaction summary).
    #[must_use]
    pub fn agent_only() -> Self {
        Self {
            agent_visible: true,
            user_visible: false,
            compacted_at: None,
            deferred_summary: None,
            focus_pinned: false,
            focus_marker_id: None,
        }
    }

    /// Message visible only to the user (e.g. compacted original).
    #[must_use]
    pub fn user_only() -> Self {
        Self {
            agent_visible: false,
            user_visible: true,
            compacted_at: None,
            deferred_summary: None,
            focus_pinned: false,
            focus_marker_id: None,
        }
    }

    /// Pinned Knowledge block — excluded from all compaction passes.
    #[must_use]
    pub fn focus_pinned() -> Self {
        Self {
            agent_visible: true,
            user_visible: false,
            compacted_at: None,
            deferred_summary: None,
            focus_pinned: true,
            focus_marker_id: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub parts: Vec<MessagePart>,
    #[serde(default)]
    pub metadata: MessageMetadata,
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
    #[must_use]
    pub fn from_legacy(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

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

    #[must_use]
    pub fn to_llm_content(&self) -> &str {
        &self.content
    }

    /// Re-synchronize `content` from `parts` after in-place mutation.
    pub fn rebuild_content(&mut self) {
        if !self.parts.is_empty() {
            self.content = Self::flatten_parts(&self.parts);
        }
    }

    fn flatten_parts(parts: &[MessagePart]) -> String {
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
                MessagePart::ThinkingBlock { .. }
                | MessagePart::RedactedThinkingBlock { .. }
                | MessagePart::Compaction { .. } => {}
            }
        }
        out
    }
}

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

    /// Whether this provider supports embedding generation.
    fn supports_embeddings(&self) -> bool;

    /// Provider name for logging and identification.
    fn name(&self) -> &str;

    /// Whether this provider supports image input (vision).
    fn supports_vision(&self) -> bool {
        false
    }

    /// Whether this provider supports native `tool_use` / function calling.
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
    #[allow(async_fn_in_trait)]
    async fn chat_with_tools(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        Ok(ChatResponse::Text(self.chat(messages).await?))
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

    /// Return the compaction summary from the most recent API call, if a server-side
    /// compaction occurred (Claude compact-2026-01-12 beta). Clears the stored value.
    fn take_compaction_summary(&self) -> Option<String> {
        None
    }

    /// Record a quality outcome from tool execution for reputation-based routing (RAPS).
    ///
    /// Only `RouterProvider` has a non-trivial implementation; all other providers are no-ops.
    /// Must only be called for semantic failures (invalid tool arguments, parse errors).
    /// Do NOT call for network errors, rate limits, or transient I/O failures.
    fn record_quality_outcome(&self, _provider_name: &str, _success: bool) {}

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
    #[cfg(feature = "schema")]
    #[allow(async_fn_in_trait)]
    async fn chat_typed<T>(&self, messages: &[Message]) -> Result<T, LlmError>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema + 'static,
        Self: Sized,
    {
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

/// Strip markdown code fences from LLM output. Only handles outer fences;
/// JSON containing trailing triple backticks in string values may be
/// incorrectly trimmed (acceptable for MVP — see review R2).
#[cfg(feature = "schema")]
fn strip_json_fences(s: &str) -> &str {
    s.trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
}

#[cfg(test)]
mod tests {
    use tokio_stream::StreamExt;

    use super::*;

    struct StubProvider {
        response: String,
    }

    impl LlmProvider for StubProvider {
        async fn chat(&self, _messages: &[Message]) -> Result<String, LlmError> {
            Ok(self.response.clone())
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

        async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
            Ok(vec![0.1, 0.2, 0.3])
        }

        fn supports_embeddings(&self) -> bool {
            false
        }

        fn name(&self) -> &'static str {
            "stub"
        }
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
        assert!(matches!(chunk, StreamChunk::Content(s) if s == "hello world"));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn chat_stream_default_propagates_chat_error() {
        struct FailProvider;

        impl LlmProvider for FailProvider {
            async fn chat(&self, _messages: &[Message]) -> Result<String, LlmError> {
                Err(LlmError::Unavailable)
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

            async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
                Err(LlmError::Unavailable)
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
            async fn chat(&self, _messages: &[Message]) -> Result<String, LlmError> {
                Err(LlmError::Unavailable)
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

            async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
                Err(LlmError::EmbedUnsupported {
                    provider: "fail".into(),
                })
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
        let text = ChatResponse::Text("hello".into());
        assert!(matches!(text, ChatResponse::Text(s) if s == "hello"));

        let tool_use = ChatResponse::ToolUse {
            text: Some("I'll run that".into()),
            tool_calls: vec![ToolUseRequest {
                id: "1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            }],
            thinking_blocks: vec![],
        };
        assert!(matches!(tool_use, ChatResponse::ToolUse { .. }));
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
        };
        let json = serde_json::to_string(&def).unwrap();
        let deserialized: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "bash");
        assert_eq!(deserialized.description, "Execute a shell command");
    }

    #[tokio::test]
    async fn supports_tool_use_default_returns_false() {
        let provider = StubProvider {
            response: String::new(),
        };
        assert!(!provider.supports_tool_use());
    }

    #[tokio::test]
    async fn chat_with_tools_default_delegates_to_chat() {
        let provider = StubProvider {
            response: "hello".into(),
        };
        let messages = vec![Message::from_legacy(Role::User, "test")];
        let result = provider.chat_with_tools(&messages, &[]).await.unwrap();
        assert!(matches!(result, ChatResponse::Text(s) if s == "hello"));
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

    #[cfg(feature = "schema")]
    #[test]
    fn strip_json_fences_plain_json() {
        assert_eq!(strip_json_fences(r#"{"a": 1}"#), r#"{"a": 1}"#);
    }

    #[cfg(feature = "schema")]
    #[test]
    fn strip_json_fences_with_json_fence() {
        assert_eq!(strip_json_fences("```json\n{\"a\": 1}\n```"), r#"{"a": 1}"#);
    }

    #[cfg(feature = "schema")]
    #[test]
    fn strip_json_fences_with_plain_fence() {
        assert_eq!(strip_json_fences("```\n{\"a\": 1}\n```"), r#"{"a": 1}"#);
    }

    #[cfg(feature = "schema")]
    #[test]
    fn strip_json_fences_whitespace() {
        assert_eq!(strip_json_fences("  \n  "), "");
    }

    #[cfg(feature = "schema")]
    #[test]
    fn strip_json_fences_empty() {
        assert_eq!(strip_json_fences(""), "");
    }

    #[cfg(feature = "schema")]
    #[test]
    fn strip_json_fences_outer_whitespace() {
        assert_eq!(
            strip_json_fences("  ```json\n{\"a\": 1}\n```  "),
            r#"{"a": 1}"#
        );
    }

    #[cfg(feature = "schema")]
    #[test]
    fn strip_json_fences_only_opening_fence() {
        assert_eq!(strip_json_fences("```json\n{\"a\": 1}"), r#"{"a": 1}"#);
    }

    // --- M27: chat_typed tests ---

    #[cfg(feature = "schema")]
    #[derive(Debug, serde::Deserialize, schemars::JsonSchema, PartialEq)]
    struct TestOutput {
        value: String,
    }

    #[cfg(feature = "schema")]
    struct SequentialStub {
        responses: std::sync::Mutex<Vec<Result<String, LlmError>>>,
    }

    #[cfg(feature = "schema")]
    impl SequentialStub {
        fn new(responses: Vec<Result<String, LlmError>>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
            }
        }
    }

    #[cfg(feature = "schema")]
    impl LlmProvider for SequentialStub {
        async fn chat(&self, _messages: &[Message]) -> Result<String, LlmError> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err(LlmError::Other("no more responses".into()));
            }
            responses.remove(0)
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

        async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
            Err(LlmError::EmbedUnsupported {
                provider: "sequential-stub".into(),
            })
        }

        fn supports_embeddings(&self) -> bool {
            false
        }

        fn name(&self) -> &'static str {
            "sequential-stub"
        }
    }

    #[cfg(feature = "schema")]
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

    #[cfg(feature = "schema")]
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

    #[cfg(feature = "schema")]
    #[tokio::test]
    async fn chat_typed_both_fail() {
        let provider = SequentialStub::new(vec![Ok("bad json".into()), Ok("still bad".into())]);
        let messages = vec![Message::from_legacy(Role::User, "test")];
        let result = provider.chat_typed::<TestOutput>(&messages).await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("parse failed after retry"));
    }

    #[cfg(feature = "schema")]
    #[tokio::test]
    async fn chat_typed_chat_error_propagates() {
        let provider = SequentialStub::new(vec![Err(LlmError::Unavailable)]);
        let messages = vec![Message::from_legacy(Role::User, "test")];
        let result = provider.chat_typed::<TestOutput>(&messages).await;
        assert!(matches!(result, Err(LlmError::Unavailable)));
    }

    #[cfg(feature = "schema")]
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
        assert!(m.agent_visible);
        assert!(m.user_visible);
        assert!(m.compacted_at.is_none());
    }

    #[test]
    fn message_metadata_agent_only() {
        let m = MessageMetadata::agent_only();
        assert!(m.agent_visible);
        assert!(!m.user_visible);
    }

    #[test]
    fn message_metadata_user_only() {
        let m = MessageMetadata::user_only();
        assert!(!m.agent_visible);
        assert!(m.user_visible);
    }

    #[test]
    fn message_metadata_serde_default() {
        let json = r#"{"role":"user","content":"hello"}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert!(msg.metadata.agent_visible);
        assert!(msg.metadata.user_visible);
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
        assert!(decoded.metadata.agent_visible);
        assert!(!decoded.metadata.user_visible);
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
        assert!(matches!(chunk, StreamChunk::Compaction(s) if s == "A summary"));
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
}
