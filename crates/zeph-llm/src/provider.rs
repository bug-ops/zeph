use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::error::LlmError;

/// Boxed stream of string chunks from an LLM provider.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<String, LlmError>> + Send>>;

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
    },
}

/// Boxed future returning an embedding vector.
pub type EmbedFuture = Pin<Box<dyn Future<Output = Result<Vec<f32>, LlmError>> + Send>>;

/// Closure type for embedding text into a vector.
pub type EmbedFn = Box<dyn Fn(&str) -> EmbedFuture + Send + Sync>;

/// Sender for emitting status events (retries, fallbacks) to the UI.
pub type StatusTx = tokio::sync::mpsc::UnboundedSender<String>;

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub parts: Vec<MessagePart>,
}

impl Message {
    #[must_use]
    pub fn from_legacy(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            parts: vec![],
        }
    }

    #[must_use]
    pub fn from_parts(role: Role, parts: Vec<MessagePart>) -> Self {
        let content = Self::flatten_parts(&parts);
        Self {
            role,
            content,
            parts,
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
                        let _ = write!(out, "[tool output: {tool_name}] (pruned)");
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
    fn name(&self) -> &'static str;

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

    /// Whether this provider supports native structured output.
    fn supports_structured_output(&self) -> bool {
        false
    }

    /// Send messages and parse the response into a typed value `T`.
    ///
    /// Default implementation injects JSON schema into the system prompt and retries once
    /// on parse failure. Providers with native structured output should override this.
    #[allow(async_fn_in_trait)]
    async fn chat_typed<T>(&self, messages: &[Message]) -> Result<T, LlmError>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema,
        Self: Sized,
    {
        let schema = schemars::schema_for!(T);
        let schema_json = serde_json::to_string_pretty(&schema)
            .map_err(|e| LlmError::StructuredParse(e.to_string()))?;
        let type_name = std::any::type_name::<T>()
            .rsplit("::")
            .next()
            .unwrap_or("Output");

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
            Ok(Box::pin(tokio_stream::once(Ok(response))))
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
        }];

        let mut stream = provider.chat_stream(&messages).await.unwrap();
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk, "hello world");
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
                Ok(Box::pin(tokio_stream::once(Ok(response))))
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
                Ok(Box::pin(tokio_stream::once(Ok(response))))
            }

            fn supports_streaming(&self) -> bool {
                false
            }

            async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
                Err(LlmError::EmbedUnsupported { provider: "fail" })
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
    fn flatten_skips_compacted_tool_output() {
        let msg = Message::from_parts(
            Role::User,
            vec![
                MessagePart::Text {
                    text: "prefix ".into(),
                },
                MessagePart::ToolOutput {
                    tool_name: "bash".into(),
                    body: "big output".into(),
                    compacted_at: Some(1234),
                },
                MessagePart::Text {
                    text: " suffix".into(),
                },
            ],
        );
        assert!(msg.content.contains("(pruned)"));
        assert!(!msg.content.contains("big output"));
        assert!(msg.content.contains("prefix "));
        assert!(msg.content.contains(" suffix"));
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
            ..
        } = msg.parts[0]
        {
            *compacted_at = Some(999);
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
        async fn chat(&self, _messages: &[Message]) -> Result<String, LlmError> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err(LlmError::Other("no more responses".into()));
            }
            responses.remove(0)
        }

        async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
            let response = self.chat(messages).await?;
            Ok(Box::pin(tokio_stream::once(Ok(response))))
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
            Err(LlmError::EmbedUnsupported {
                provider: "sequential-stub",
            })
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
        assert!(matches!(result, Err(LlmError::Unavailable)));
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
}
