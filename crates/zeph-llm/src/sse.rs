use eventsource_stream::Eventsource;
use serde::Deserialize;
use tokio_stream::StreamExt;

use crate::error::LlmError;
use crate::provider::ChatStream;

/// Convert a Claude streaming response into a `ChatStream`.
pub(crate) fn claude_sse_to_stream(response: reqwest::Response) -> ChatStream {
    let event_stream = response.bytes_stream().eventsource();
    let mapped = event_stream.filter_map(|event| match event {
        Ok(event) => parse_claude_sse_event(&event.data, &event.event),
        Err(e) => Some(Err(LlmError::SseParse(e.to_string()))),
    });
    Box::pin(mapped)
}

/// Convert an `OpenAI` streaming response into a `ChatStream`.
pub(crate) fn openai_sse_to_stream(response: reqwest::Response) -> ChatStream {
    let event_stream = response.bytes_stream().eventsource();
    let mapped = event_stream.filter_map(|event| match event {
        Ok(event) => parse_openai_sse_event(&event.data),
        Err(e) => Some(Err(LlmError::SseParse(e.to_string()))),
    });
    Box::pin(mapped)
}

fn parse_claude_sse_event(data: &str, event_type: &str) -> Option<Result<String, LlmError>> {
    match event_type {
        "content_block_delta" => match serde_json::from_str::<ClaudeStreamEvent>(data) {
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
        "error" => match serde_json::from_str::<ClaudeStreamEvent>(data) {
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

fn parse_openai_sse_event(data: &str) -> Option<Result<String, LlmError>> {
    if data == "[DONE]" {
        return None;
    }

    match serde_json::from_str::<OpenAiStreamChunk>(data) {
        Ok(chunk) => {
            let content = chunk
                .choices
                .first()
                .and_then(|c| c.delta.content.as_deref())
                .unwrap_or_default();

            if content.is_empty() {
                None
            } else {
                Some(Ok(content.to_owned()))
            }
        }
        Err(e) => Some(Err(LlmError::SseParse(format!(
            "failed to parse SSE data: {e}"
        )))),
    }
}

#[derive(Deserialize)]
struct ClaudeStreamEvent {
    #[serde(default)]
    delta: Option<ClaudeDelta>,
    #[serde(default)]
    error: Option<ClaudeStreamError>,
}

#[derive(Deserialize)]
struct ClaudeDelta {
    #[serde(rename = "type")]
    delta_type: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct ClaudeStreamError {
    #[serde(rename = "type")]
    error_type: String,
    message: String,
}

#[derive(Deserialize)]
struct OpenAiStreamChunk {
    choices: Vec<OpenAiStreamChoice>,
}

#[derive(Deserialize)]
struct OpenAiStreamChoice {
    delta: OpenAiStreamDelta,
}

#[derive(Deserialize)]
struct OpenAiStreamDelta {
    #[serde(default)]
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_parse_text_delta() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let result = parse_claude_sse_event(data, "content_block_delta");
        assert_eq!(result.unwrap().unwrap(), "Hello");
    }

    #[test]
    fn claude_parse_empty_text_delta() {
        let data =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#;
        let result = parse_claude_sse_event(data, "content_block_delta");
        assert!(result.is_none());
    }

    #[test]
    fn claude_parse_error_event() {
        let data = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let result = parse_claude_sse_event(data, "error");
        let err = result.unwrap().unwrap_err();
        assert!(err.to_string().contains("overloaded_error"));
    }

    #[test]
    fn claude_parse_unknown_event_skipped() {
        let result = parse_claude_sse_event("{}", "ping");
        assert!(result.is_none());
    }

    #[test]
    fn openai_parse_text_chunk() {
        let data = r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}"#;
        let result = parse_openai_sse_event(data);
        assert_eq!(result.unwrap().unwrap(), "hi");
    }

    #[test]
    fn openai_parse_done_signal() {
        let result = parse_openai_sse_event("[DONE]");
        assert!(result.is_none());
    }

    #[test]
    fn openai_parse_empty_content() {
        let data = r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
        let result = parse_openai_sse_event(data);
        assert!(result.is_none());
    }

    #[test]
    fn openai_parse_invalid_json() {
        let result = parse_openai_sse_event("not json");
        let err = result.unwrap().unwrap_err();
        assert!(err.to_string().contains("failed to parse SSE data"));
    }
}
