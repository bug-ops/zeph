// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use eventsource_stream::Eventsource;
use serde::Deserialize;
use tokio_stream::StreamExt;

use crate::error::LlmError;
use crate::provider::{ChatStream, StreamChunk};

/// Convert a Claude streaming response into a `ChatStream`.
pub(crate) fn claude_sse_to_stream(response: reqwest::Response) -> ChatStream {
    let event_stream = response.bytes_stream().eventsource();
    let mapped = event_stream.filter_map(|event| match event {
        Ok(event) => parse_claude_sse_event(&event.data, &event.event),
        Err(e) => Some(Err(LlmError::SseParse(e.to_string()))),
    });
    Box::pin(mapped)
}

/// Convert a Gemini streaming response into a `ChatStream`.
pub(crate) fn gemini_sse_to_stream(response: reqwest::Response) -> ChatStream {
    let event_stream = response.bytes_stream().eventsource();
    let mapped = event_stream.filter_map(|event| match event {
        Ok(event) => parse_gemini_sse_event(&event.data),
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

fn parse_claude_sse_event(data: &str, event_type: &str) -> Option<Result<StreamChunk, LlmError>> {
    match event_type {
        "content_block_delta" => match serde_json::from_str::<ClaudeStreamEvent>(data) {
            Ok(event) => {
                if let Some(delta) = event.delta {
                    match delta.delta_type.as_str() {
                        "text_delta" if !delta.text.is_empty() => {
                            return Some(Ok(StreamChunk::Content(delta.text)));
                        }
                        "thinking_delta" if !delta.thinking.is_empty() => {
                            return Some(Ok(StreamChunk::Thinking(delta.thinking)));
                        }
                        "signature_delta" => {
                            tracing::debug!("Claude signature_delta (not emitted to stream)");
                        }
                        _ => {}
                    }
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

fn parse_openai_sse_event(data: &str) -> Option<Result<StreamChunk, LlmError>> {
    if data == "[DONE]" {
        return None;
    }

    match serde_json::from_str::<OpenAiStreamChunk>(data) {
        Ok(chunk) => {
            let choice = chunk.choices.first()?;
            let reasoning = choice
                .delta
                .reasoning_content
                .as_deref()
                .unwrap_or_default();
            if !reasoning.is_empty() {
                return Some(Ok(StreamChunk::Thinking(reasoning.to_owned())));
            }
            let content = choice.delta.content.as_deref().unwrap_or_default();
            if content.is_empty() {
                None
            } else {
                Some(Ok(StreamChunk::Content(content.to_owned())))
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
    #[serde(default)]
    thinking: String,
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
    #[serde(default)]
    reasoning_content: Option<String>,
}

fn parse_gemini_sse_event(data: &str) -> Option<Result<StreamChunk, LlmError>> {
    let resp: GeminiStreamResponse = match serde_json::from_str(data) {
        Ok(r) => r,
        Err(e) => {
            return Some(Err(LlmError::SseParse(format!(
                "failed to parse Gemini SSE data: {e}"
            ))));
        }
    };

    let parts = resp.candidates.first()?.content.as_ref()?.parts.as_slice();

    // Collect thinking and content text separately (HIGH-1 fix: handle multi-part events).
    let mut thinking = String::new();
    let mut content = String::new();
    for part in parts {
        if let Some(text) = part.text.as_deref()
            && !text.is_empty()
        {
            if part.thought == Some(true) {
                thinking.push_str(text);
            } else {
                content.push_str(text);
            }
        }
    }

    // Prioritize thinking over content (mirrors OpenAI reasoning_content handling).
    if !thinking.is_empty() {
        Some(Ok(StreamChunk::Thinking(thinking)))
    } else if !content.is_empty() {
        Some(Ok(StreamChunk::Content(content)))
    } else {
        None
    }
}

#[derive(Deserialize)]
struct GeminiStreamResponse {
    #[serde(default)]
    candidates: Vec<GeminiStreamCandidate>,
}

#[derive(Deserialize)]
struct GeminiStreamCandidate {
    content: Option<GeminiStreamContent>,
}

#[derive(Deserialize)]
struct GeminiStreamContent {
    #[serde(default)]
    parts: Vec<GeminiStreamPart>,
}

#[derive(Deserialize)]
struct GeminiStreamPart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_parse_text_delta() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let result = parse_claude_sse_event(data, "content_block_delta");
        let chunk = result.unwrap().unwrap();
        assert!(matches!(chunk, StreamChunk::Content(s) if s == "Hello"));
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
        let chunk = result.unwrap().unwrap();
        assert!(matches!(chunk, StreamChunk::Content(s) if s == "hi"));
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

    #[test]
    fn claude_thinking_delta_emitted_as_thinking_chunk() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I need to think about this"}}"#;
        let result = parse_claude_sse_event(data, "content_block_delta");
        let chunk = result.unwrap().unwrap();
        assert!(matches!(chunk, StreamChunk::Thinking(s) if s == "I need to think about this"));
    }

    #[test]
    fn claude_thinking_delta_empty_not_emitted() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#;
        let result = parse_claude_sse_event(data, "content_block_delta");
        assert!(result.is_none());
    }

    #[test]
    fn openai_parse_reasoning_content_chunk() {
        let data =
            r#"{"choices":[{"delta":{"reasoning_content":"Let me reason"},"finish_reason":null}]}"#;
        let result = parse_openai_sse_event(data);
        let chunk = result.unwrap().unwrap();
        assert!(matches!(chunk, StreamChunk::Thinking(s) if s == "Let me reason"));
    }

    #[test]
    fn claude_signature_delta_not_emitted_to_stream() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc123"}}"#;
        let result = parse_claude_sse_event(data, "content_block_delta");
        assert!(result.is_none());
    }

    #[test]
    fn gemini_parse_text_chunk() {
        let data = r#"{"candidates":[{"content":{"parts":[{"text":"Hello"}]}}]}"#;
        let result = parse_gemini_sse_event(data);
        let chunk = result.unwrap().unwrap();
        assert!(matches!(chunk, StreamChunk::Content(s) if s == "Hello"));
    }

    #[test]
    fn gemini_parse_thinking_chunk() {
        let data =
            r#"{"candidates":[{"content":{"parts":[{"text":"Let me think","thought":true}]}}]}"#;
        let result = parse_gemini_sse_event(data);
        let chunk = result.unwrap().unwrap();
        assert!(matches!(chunk, StreamChunk::Thinking(s) if s == "Let me think"));
    }

    #[test]
    fn gemini_parse_empty_text_skipped() {
        let data = r#"{"candidates":[{"content":{"parts":[{"text":""}]}}]}"#;
        let result = parse_gemini_sse_event(data);
        assert!(result.is_none());
    }

    #[test]
    fn gemini_parse_no_candidates_skipped() {
        let data = r#"{"candidates":[]}"#;
        let result = parse_gemini_sse_event(data);
        assert!(result.is_none());
    }

    #[test]
    fn gemini_parse_invalid_json_error() {
        let result = parse_gemini_sse_event("not json");
        let err = result.unwrap().unwrap_err();
        assert!(err.to_string().contains("failed to parse Gemini SSE data"));
    }

    #[test]
    fn gemini_parse_thought_false_emitted_as_content() {
        let data = r#"{"candidates":[{"content":{"parts":[{"text":"Regular","thought":false}]}}]}"#;
        let result = parse_gemini_sse_event(data);
        let chunk = result.unwrap().unwrap();
        assert!(matches!(chunk, StreamChunk::Content(s) if s == "Regular"));
    }

    #[test]
    fn gemini_parse_multi_part_thinking_priority() {
        // Multi-part event with both thinking and content parts — thinking takes priority.
        let data = r#"{"candidates":[{"content":{"parts":[{"text":"reasoning","thought":true},{"text":"answer"}]}}]}"#;
        let result = parse_gemini_sse_event(data);
        let chunk = result.unwrap().unwrap();
        assert!(matches!(chunk, StreamChunk::Thinking(s) if s == "reasoning"));
    }
}
