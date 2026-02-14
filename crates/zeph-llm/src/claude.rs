use std::fmt;
use std::time::Duration;

use crate::error::LlmError;
use eventsource_stream::Eventsource;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;

use crate::provider::{ChatStream, LlmProvider, Message, Role, StatusTx};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_RETRIES: u32 = 3;
const BASE_BACKOFF_SECS: u64 = 1;

pub struct ClaudeProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    pub(crate) status_tx: Option<StatusTx>,
}

impl fmt::Debug for ClaudeProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeProvider")
            .field("client", &"<reqwest::Client>")
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("status_tx", &self.status_tx.is_some())
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
            status_tx: self.status_tx.clone(),
        }
    }
}

impl ClaudeProvider {
    #[must_use]
    pub fn new(api_key: String, model: String, max_tokens: u32) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            max_tokens,
            status_tx: None,
        }
    }

    #[must_use]
    pub fn with_status_tx(mut self, tx: StatusTx) -> Self {
        self.status_tx = Some(tx);
        self
    }

    fn emit_status(&self, msg: impl Into<String>) {
        if let Some(ref tx) = self.status_tx {
            let _ = tx.send(msg.into());
        }
    }

    fn build_request(&self, messages: &[Message], stream: bool) -> reqwest::RequestBuilder {
        let (system, chat_messages) = split_messages(messages);

        let body = RequestBody {
            model: &self.model,
            max_tokens: self.max_tokens,
            system: system.as_deref(),
            messages: &chat_messages,
            stream,
        };

        self.client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
    }

    async fn send_request(&self, messages: &[Message]) -> Result<String, LlmError> {
        for attempt in 0..=MAX_RETRIES {
            let response = self.build_request(messages, false).send().await?;

            let status = response.status();

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                if attempt == MAX_RETRIES {
                    return Err(LlmError::RateLimited);
                }
                let delay = retry_delay(&response, attempt);
                self.emit_status(format!(
                    "Claude rate limited, retrying in {}s ({}/{})",
                    delay.as_secs(),
                    attempt + 1,
                    MAX_RETRIES
                ));
                tracing::warn!(
                    "Claude rate limited, retrying in {}s (attempt {}/{})",
                    delay.as_secs(),
                    attempt + 1,
                    MAX_RETRIES
                );
                tokio::time::sleep(delay).await;
                continue;
            }

            let text = response.text().await.map_err(LlmError::Http)?;

            if !status.is_success() {
                tracing::error!("Claude API error {status}: {text}");
                return Err(LlmError::Other(format!(
                    "Claude API request failed (status {status})"
                )));
            }

            let resp: ApiResponse = serde_json::from_str(&text)?;

            return resp
                .content
                .first()
                .map(|c| c.text.clone())
                .ok_or(LlmError::EmptyResponse { provider: "claude" });
        }

        Err(LlmError::RateLimited)
    }

    async fn send_stream_request(
        &self,
        messages: &[Message],
    ) -> Result<reqwest::Response, LlmError> {
        for attempt in 0..=MAX_RETRIES {
            let response = self.build_request(messages, true).send().await?;

            let status = response.status();

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                if attempt == MAX_RETRIES {
                    return Err(LlmError::RateLimited);
                }
                let delay = retry_delay(&response, attempt);
                self.emit_status(format!(
                    "Claude rate limited, retrying in {}s ({}/{})",
                    delay.as_secs(),
                    attempt + 1,
                    MAX_RETRIES
                ));
                tracing::warn!(
                    "Claude rate limited, retrying in {}s (attempt {}/{})",
                    delay.as_secs(),
                    attempt + 1,
                    MAX_RETRIES
                );
                tokio::time::sleep(delay).await;
                continue;
            }

            if !status.is_success() {
                let text = response.text().await.map_err(LlmError::Http)?;
                tracing::error!("Claude API streaming request error {status}: {text}");
                return Err(LlmError::Other(format!(
                    "Claude API streaming request failed (status {status})"
                )));
            }

            return Ok(response);
        }

        Err(LlmError::RateLimited)
    }
}

impl LlmProvider for ClaudeProvider {
    fn context_window(&self) -> Option<usize> {
        if self.model.contains("opus")
            || self.model.contains("sonnet")
            || self.model.contains("haiku")
        {
            Some(200_000)
        } else {
            None
        }
    }

    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        self.send_request(messages).await
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        let response = self.send_stream_request(messages).await?;

        let event_stream = response.bytes_stream().eventsource();

        let mapped = event_stream.filter_map(|event| match event {
            Ok(event) => parse_sse_event(&event.data, &event.event),
            Err(e) => Some(Err(LlmError::SseParse(e.to_string()))),
        });

        Ok(Box::pin(mapped))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
        Err(LlmError::EmbedUnsupported { provider: "claude" })
    }

    fn supports_embeddings(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "claude"
    }
}

fn retry_delay(response: &reqwest::Response, attempt: u32) -> Duration {
    if let Some(val) = response.headers().get("retry-after")
        && let Ok(s) = val.to_str()
        && let Ok(secs) = s.parse::<u64>()
    {
        return Duration::from_secs(secs);
    }
    Duration::from_secs(BASE_BACKOFF_SECS << attempt)
}

fn parse_sse_event(data: &str, event_type: &str) -> Option<Result<String, LlmError>> {
    match event_type {
        "content_block_delta" => match serde_json::from_str::<StreamEvent>(data) {
            Ok(event) => {
                if let Some(delta) = event.delta
                    && delta.delta_type == "text_delta"
                    && !delta.text.is_empty()
                {
                    return Some(Ok(delta.text));
                }
                None
            }
            Err(e) => Some(Err(LlmError::SseParse(format!(
                "failed to parse SSE data: {e}"
            )))),
        },
        "error" => match serde_json::from_str::<StreamEvent>(data) {
            Ok(event) => {
                if let Some(err) = event.error {
                    Some(Err(LlmError::SseParse(format!(
                        "Claude stream error ({}): {}",
                        err.error_type, err.message
                    ))))
                } else {
                    Some(Err(LlmError::SseParse(format!(
                        "Claude stream error: {data}"
                    ))))
                }
            }
            Err(_) => Some(Err(LlmError::SseParse(format!(
                "Claude stream error: {data}"
            )))),
        },
        _ => None,
    }
}

fn split_messages(messages: &[Message]) -> (Option<String>, Vec<ApiMessage<'_>>) {
    let mut system_parts = Vec::new();
    let mut chat = Vec::new();

    for msg in messages {
        match msg.role {
            Role::System => system_parts.push(msg.to_llm_content()),
            Role::User => chat.push(ApiMessage {
                role: "user",
                content: msg.to_llm_content(),
            }),
            Role::Assistant => chat.push(ApiMessage {
                role: "assistant",
                content: msg.to_llm_content(),
            }),
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    (system, chat)
}

#[derive(Serialize)]
struct RequestBody<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: &'a [ApiMessage<'a>],
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    text: String,
}

#[derive(Deserialize)]
struct StreamEvent {
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    error: Option<StreamError>,
}

#[derive(Deserialize)]
struct Delta {
    #[serde(rename = "type")]
    delta_type: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct StreamError {
    #[serde(rename = "type")]
    error_type: String,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_window_known_models() {
        let sonnet = ClaudeProvider::new("k".into(), "claude-sonnet-4-5-20250929".into(), 1024);
        assert_eq!(sonnet.context_window(), Some(200_000));

        let opus = ClaudeProvider::new("k".into(), "claude-opus-4-6".into(), 1024);
        assert_eq!(opus.context_window(), Some(200_000));

        let haiku = ClaudeProvider::new("k".into(), "claude-haiku-4-5".into(), 1024);
        assert_eq!(haiku.context_window(), Some(200_000));
    }

    #[test]
    fn context_window_unknown_model() {
        let provider = ClaudeProvider::new("k".into(), "unknown-model".into(), 1024);
        assert!(provider.context_window().is_none());
    }

    #[test]
    fn split_messages_extracts_system() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "You are helpful.".into(),
                parts: vec![],
            },
            Message {
                role: Role::User,
                content: "Hi".into(),
                parts: vec![],
            },
        ];

        let (system, chat) = split_messages(&messages);
        assert_eq!(system.unwrap(), "You are helpful.");
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].role, "user");
    }

    #[test]
    fn split_messages_no_system() {
        let messages = vec![Message {
            role: Role::User,
            content: "Hi".into(),
            parts: vec![],
        }];

        let (system, chat) = split_messages(&messages);
        assert!(system.is_none());
        assert_eq!(chat.len(), 1);
    }

    #[test]
    fn split_messages_multiple_system() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "Part 1".into(),
                parts: vec![],
            },
            Message {
                role: Role::System,
                content: "Part 2".into(),
                parts: vec![],
            },
            Message {
                role: Role::User,
                content: "Hi".into(),
                parts: vec![],
            },
        ];

        let (system, _) = split_messages(&messages);
        assert_eq!(system.unwrap(), "Part 1\n\nPart 2");
    }

    #[test]
    fn parse_sse_event_text_delta() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let result = parse_sse_event(data, "content_block_delta");
        assert_eq!(result.unwrap().unwrap(), "Hello");
    }

    #[test]
    fn parse_sse_event_empty_text_delta() {
        let data =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#;
        let result = parse_sse_event(data, "content_block_delta");
        assert!(result.is_none());
    }

    #[test]
    fn parse_sse_event_error() {
        let data = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let result = parse_sse_event(data, "error");
        let err = result.unwrap().unwrap_err();
        assert!(err.to_string().contains("overloaded_error"));
        assert!(err.to_string().contains("Overloaded"));
    }

    #[test]
    fn parse_sse_event_message_start_skipped() {
        let data = r#"{"type":"message_start","message":{}}"#;
        let result = parse_sse_event(data, "message_start");
        assert!(result.is_none());
    }

    #[test]
    fn parse_sse_event_message_stop_skipped() {
        let data = r#"{"type":"message_stop"}"#;
        let result = parse_sse_event(data, "message_stop");
        assert!(result.is_none());
    }

    #[test]
    fn parse_sse_event_ping_skipped() {
        let result = parse_sse_event("{}", "ping");
        assert!(result.is_none());
    }

    #[test]
    fn parse_sse_event_invalid_json() {
        let result = parse_sse_event("not json", "content_block_delta");
        let err = result.unwrap().unwrap_err();
        assert!(err.to_string().contains("failed to parse SSE data"));
    }

    #[test]
    fn supports_streaming_returns_true() {
        let provider =
            ClaudeProvider::new("test-key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
        assert!(provider.supports_streaming());
    }

    #[test]
    fn debug_redacts_api_key() {
        let provider = ClaudeProvider::new(
            "sk-secret-key".into(),
            "claude-sonnet-4-5-20250929".into(),
            1024,
        );
        let debug_output = format!("{provider:?}");
        assert!(!debug_output.contains("sk-secret-key"));
        assert!(debug_output.contains("<redacted>"));
        assert!(debug_output.contains("claude-sonnet-4-5-20250929"));
    }

    #[test]
    fn claude_supports_embeddings_returns_false() {
        let provider =
            ClaudeProvider::new("test-key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
        assert!(!provider.supports_embeddings());
    }

    #[tokio::test]
    async fn claude_embed_returns_error() {
        let provider =
            ClaudeProvider::new("test-key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
        let result = provider.embed("test").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("embedding not supported by claude")
        );
    }

    #[test]
    fn name_returns_claude() {
        let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
        assert_eq!(provider.name(), "claude");
    }

    #[test]
    fn clone_preserves_fields() {
        let provider = ClaudeProvider::new(
            "test-api-key".into(),
            "claude-sonnet-4-5-20250929".into(),
            2048,
        );
        let cloned = provider.clone();
        assert_eq!(cloned.model, provider.model);
        assert_eq!(cloned.api_key, provider.api_key);
        assert_eq!(cloned.max_tokens, provider.max_tokens);
    }

    #[test]
    fn new_stores_fields_correctly() {
        let provider = ClaudeProvider::new("my-key".into(), "claude-haiku-35".into(), 4096);
        assert_eq!(provider.api_key, "my-key");
        assert_eq!(provider.model, "claude-haiku-35");
        assert_eq!(provider.max_tokens, 4096);
    }

    #[test]
    fn debug_includes_model_and_max_tokens() {
        let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-5-20250929".into(), 512);
        let debug = format!("{provider:?}");
        assert!(debug.contains("ClaudeProvider"));
        assert!(debug.contains("512"));
        assert!(debug.contains("<reqwest::Client>"));
    }

    #[test]
    fn request_body_serializes_without_system() {
        let body = RequestBody {
            model: "claude-sonnet-4-5-20250929",
            max_tokens: 1024,
            system: None,
            messages: &[ApiMessage {
                role: "user",
                content: "hello",
            }],
            stream: false,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(!json.contains("system"));
        assert!(!json.contains("stream"));
        assert!(json.contains("\"model\":\"claude-sonnet-4-5-20250929\""));
        assert!(json.contains("\"max_tokens\":1024"));
    }

    #[test]
    fn request_body_serializes_with_system() {
        let body = RequestBody {
            model: "claude-sonnet-4-5-20250929",
            max_tokens: 1024,
            system: Some("You are helpful."),
            messages: &[],
            stream: false,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"system\":\"You are helpful.\""));
    }

    #[test]
    fn request_body_serializes_stream_true() {
        let body = RequestBody {
            model: "test",
            max_tokens: 100,
            system: None,
            messages: &[],
            stream: true,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"stream\":true"));
    }

    #[test]
    fn split_messages_all_roles() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "system prompt".into(),
                parts: vec![],
            },
            Message {
                role: Role::User,
                content: "user msg".into(),
                parts: vec![],
            },
            Message {
                role: Role::Assistant,
                content: "assistant reply".into(),
                parts: vec![],
            },
            Message {
                role: Role::User,
                content: "followup".into(),
                parts: vec![],
            },
        ];
        let (system, chat) = split_messages(&messages);
        assert_eq!(system.unwrap(), "system prompt");
        assert_eq!(chat.len(), 3);
        assert_eq!(chat[0].role, "user");
        assert_eq!(chat[0].content, "user msg");
        assert_eq!(chat[1].role, "assistant");
        assert_eq!(chat[1].content, "assistant reply");
        assert_eq!(chat[2].role, "user");
        assert_eq!(chat[2].content, "followup");
    }

    #[test]
    fn split_messages_empty() {
        let (system, chat) = split_messages(&[]);
        assert!(system.is_none());
        assert!(chat.is_empty());
    }

    #[test]
    fn parse_sse_error_without_structured_error() {
        let data = r#"not valid json at all"#;
        let result = parse_sse_event(data, "error");
        let err = result.unwrap().unwrap_err();
        assert!(err.to_string().contains("Claude stream error"));
    }

    #[test]
    fn parse_sse_error_with_empty_error_field() {
        let data = r#"{"type":"error"}"#;
        let result = parse_sse_event(data, "error");
        let err = result.unwrap().unwrap_err();
        assert!(err.to_string().contains("Claude stream error"));
    }

    #[test]
    fn parse_sse_content_block_delta_non_text_type() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#;
        let result = parse_sse_event(data, "content_block_delta");
        assert!(result.is_none());
    }

    #[test]
    fn parse_sse_content_block_delta_no_delta() {
        let data = r#"{"type":"content_block_delta","index":0}"#;
        let result = parse_sse_event(data, "content_block_delta");
        assert!(result.is_none());
    }

    #[test]
    fn parse_sse_content_block_start_skipped() {
        let data = r#"{"type":"content_block_start","index":0}"#;
        let result = parse_sse_event(data, "content_block_start");
        assert!(result.is_none());
    }

    #[test]
    fn parse_sse_content_block_stop_skipped() {
        let data = r#"{"type":"content_block_stop","index":0}"#;
        let result = parse_sse_event(data, "content_block_stop");
        assert!(result.is_none());
    }

    #[test]
    fn parse_sse_message_delta_skipped() {
        let data = r#"{"type":"message_delta","usage":{}}"#;
        let result = parse_sse_event(data, "message_delta");
        assert!(result.is_none());
    }

    #[test]
    fn parse_sse_error_invalid_json() {
        let result = parse_sse_event("{broken", "error");
        let err = result.unwrap().unwrap_err();
        assert!(err.to_string().contains("Claude stream error"));
    }

    #[test]
    fn stream_event_deserializes_with_delta() {
        let json = r#"{"delta":{"type":"text_delta","text":"hi"}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        let delta = event.delta.unwrap();
        assert_eq!(delta.delta_type, "text_delta");
        assert_eq!(delta.text, "hi");
    }

    #[test]
    fn stream_event_deserializes_with_error() {
        let json = r#"{"error":{"type":"rate_limit","message":"too fast"}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        let err = event.error.unwrap();
        assert_eq!(err.error_type, "rate_limit");
        assert_eq!(err.message, "too fast");
    }

    #[test]
    fn stream_event_deserializes_empty() {
        let json = r#"{}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        assert!(event.delta.is_none());
        assert!(event.error.is_none());
    }

    #[test]
    fn delta_default_text_is_empty() {
        let json = r#"{"type":"text_delta"}"#;
        let delta: Delta = serde_json::from_str(json).unwrap();
        assert_eq!(delta.delta_type, "text_delta");
        assert!(delta.text.is_empty());
    }

    #[test]
    fn api_message_serializes() {
        let msg = ApiMessage {
            role: "user",
            content: "hello world",
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"hello world\""));
    }

    #[test]
    fn content_block_deserializes() {
        let json = r#"{"text":"response text"}"#;
        let block: ContentBlock = serde_json::from_str(json).unwrap();
        assert_eq!(block.text, "response text");
    }

    #[test]
    fn api_response_multiple_content_blocks() {
        let json = r#"{"content":[{"text":"first"},{"text":"second"}]}"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.content.len(), 2);
        assert_eq!(resp.content[0].text, "first");
        assert_eq!(resp.content[1].text, "second");
    }

    #[tokio::test]
    async fn chat_with_unreachable_endpoint_errors() {
        let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
        let messages = vec![Message {
            role: Role::User,
            content: "test".into(),
            parts: vec![],
        }];
        let result = provider.chat(&messages).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn chat_stream_with_unreachable_endpoint_errors() {
        let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
        let messages = vec![Message {
            role: Role::User,
            content: "test".into(),
            parts: vec![],
        }];
        let result = provider.chat_stream(&messages).await;
        assert!(result.is_err());
    }

    #[test]
    fn split_messages_only_system() {
        let messages = vec![Message {
            role: Role::System,
            content: "instruction".into(),
            parts: vec![],
        }];
        let (system, chat) = split_messages(&messages);
        assert_eq!(system.unwrap(), "instruction");
        assert!(chat.is_empty());
    }

    #[test]
    fn split_messages_only_assistant() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: "reply".into(),
            parts: vec![],
        }];
        let (system, chat) = split_messages(&messages);
        assert!(system.is_none());
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].role, "assistant");
    }

    #[test]
    fn split_messages_interleaved_system() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "first".into(),
                parts: vec![],
            },
            Message {
                role: Role::User,
                content: "question".into(),
                parts: vec![],
            },
            Message {
                role: Role::System,
                content: "second".into(),
                parts: vec![],
            },
        ];
        let (system, chat) = split_messages(&messages);
        assert_eq!(system.unwrap(), "first\n\nsecond");
        assert_eq!(chat.len(), 1);
    }

    #[test]
    fn request_body_serializes_with_stream_false_omits_stream() {
        let body = RequestBody {
            model: "test",
            max_tokens: 100,
            system: None,
            messages: &[],
            stream: false,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(!json.contains("stream"));
    }

    #[test]
    fn api_response_deserializes() {
        let json = r#"{"content":[{"text":"Hello world"}]}"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.content.len(), 1);
        assert_eq!(resp.content[0].text, "Hello world");
    }

    #[test]
    fn api_response_empty_content() {
        let json = r#"{"content":[]}"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert!(resp.content.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires ZEPH_CLAUDE_API_KEY env var"]
    async fn integration_claude_chat() {
        let api_key =
            std::env::var("ZEPH_CLAUDE_API_KEY").expect("ZEPH_CLAUDE_API_KEY must be set");
        let provider = ClaudeProvider::new(api_key, "claude-sonnet-4-5-20250929".into(), 256);

        let messages = vec![Message {
            role: Role::User,
            content: "Reply with exactly: pong".into(),
            parts: vec![],
        }];

        let response = provider.chat(&messages).await.unwrap();
        assert!(response.to_lowercase().contains("pong"));
    }

    #[tokio::test]
    #[ignore = "requires ZEPH_CLAUDE_API_KEY env var"]
    async fn integration_claude_chat_stream() {
        let api_key =
            std::env::var("ZEPH_CLAUDE_API_KEY").expect("ZEPH_CLAUDE_API_KEY must be set");
        let provider = ClaudeProvider::new(api_key, "claude-sonnet-4-5-20250929".into(), 256);

        let messages = vec![Message {
            role: Role::User,
            content: "Reply with exactly: pong".into(),
            parts: vec![],
        }];

        let mut stream = provider.chat_stream(&messages).await.unwrap();
        let mut chunks = Vec::new();
        let mut chunk_count = 0;

        while let Some(result) = stream.next().await {
            let chunk = result.unwrap();
            chunks.push(chunk);
            chunk_count += 1;
        }

        let full_response: String = chunks.concat();
        assert!(!full_response.is_empty());
        assert!(full_response.to_lowercase().contains("pong"));
        assert!(chunk_count >= 1);
    }

    #[tokio::test]
    #[ignore = "requires ZEPH_CLAUDE_API_KEY env var"]
    async fn integration_claude_stream_matches_chat() {
        let api_key =
            std::env::var("ZEPH_CLAUDE_API_KEY").expect("ZEPH_CLAUDE_API_KEY must be set");
        let provider = ClaudeProvider::new(api_key, "claude-sonnet-4-5-20250929".into(), 256);

        let messages = vec![Message {
            role: Role::User,
            content: "What is 2+2? Reply with just the number.".into(),
            parts: vec![],
        }];

        let chat_response = provider.chat(&messages).await.unwrap();

        let mut stream = provider.chat_stream(&messages).await.unwrap();
        let mut stream_chunks = Vec::new();
        while let Some(result) = stream.next().await {
            stream_chunks.push(result.unwrap());
        }
        let stream_response: String = stream_chunks.concat();

        assert!(chat_response.contains('4'));
        assert!(stream_response.contains('4'));
    }

    #[test]
    fn backoff_constants() {
        assert_eq!(MAX_RETRIES, 3);
        assert_eq!(BASE_BACKOFF_SECS, 1);
        // exponential: 1s, 2s, 4s
        assert_eq!(
            Duration::from_secs(BASE_BACKOFF_SECS << 0),
            Duration::from_secs(1)
        );
        assert_eq!(
            Duration::from_secs(BASE_BACKOFF_SECS << 1),
            Duration::from_secs(2)
        );
        assert_eq!(
            Duration::from_secs(BASE_BACKOFF_SECS << 2),
            Duration::from_secs(4)
        );
    }
}
