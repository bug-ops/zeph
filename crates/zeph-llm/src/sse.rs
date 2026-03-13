// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use eventsource_stream::Eventsource;
use serde::Deserialize;
use tokio_stream::StreamExt;

use crate::error::LlmError;
use crate::provider::{ChatStream, StreamChunk};

/// State machine for accumulating multi-event Claude SSE blocks (e.g. compaction).
#[derive(Default)]
struct ClaudeSseState {
    /// When `Some`, we are accumulating a compaction block. Holds the summary text so far.
    compaction_buf: Option<String>,
}

/// Convert a Claude streaming response into a `ChatStream`.
pub(crate) fn claude_sse_to_stream(response: reqwest::Response) -> ChatStream {
    let event_stream = response.bytes_stream().eventsource();
    let s = async_stream::stream! {
        let mut state = ClaudeSseState::default();
        let mut pinned = std::pin::pin!(event_stream);
        while let Some(event) = pinned.next().await {
            match event {
                Ok(ev) => {
                    if let Some(chunk) = parse_claude_sse_event(&mut state, &ev.data, &ev.event) {
                        yield chunk;
                    }
                }
                Err(e) => yield Err(LlmError::SseParse(e.to_string())),
            }
        }
    };
    Box::pin(s)
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

fn parse_claude_sse_event(
    state: &mut ClaudeSseState,
    data: &str,
    event_type: &str,
) -> Option<Result<StreamChunk, LlmError>> {
    match event_type {
        "content_block_start" => {
            // Detect a compaction block starting.
            match serde_json::from_str::<ClaudeContentBlockStart>(data) {
                Ok(ev) if ev.content_block.block_type == "compaction" => {
                    tracing::debug!("Claude compaction block started");
                    state.compaction_buf = Some(String::new());
                }
                _ => {}
            }
            None
        }
        "content_block_stop" => {
            // If we were accumulating a compaction block, emit it now.
            if let Some(summary) = state.compaction_buf.take() {
                tracing::info!(
                    summary_len = summary.len(),
                    "Claude server-side compaction block completed in stream"
                );
                return Some(Ok(StreamChunk::Compaction(summary)));
            }
            None
        }
        "content_block_delta" => match serde_json::from_str::<ClaudeStreamEvent>(data) {
            Ok(event) => {
                if let Some(delta) = event.delta {
                    match delta.delta_type.as_str() {
                        "text_delta" if !delta.text.is_empty() => {
                            // If inside a compaction block, accumulate into buffer (32 KiB cap).
                            if let Some(ref mut buf) = state.compaction_buf {
                                const MAX_COMPACTION_BUF: usize = 32 * 1024;
                                let remaining = MAX_COMPACTION_BUF.saturating_sub(buf.len());
                                if remaining == 0 {
                                    tracing::warn!(
                                        "compaction buffer exceeded 32 KiB cap; discarding excess"
                                    );
                                } else {
                                    let to_append = &delta.text[..delta.text.len().min(remaining)];
                                    buf.push_str(to_append);
                                }
                                return None;
                            }
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

/// Used for `content_block_start` events to detect compaction blocks.
#[derive(Deserialize)]
struct ClaudeContentBlockStart {
    content_block: ClaudeContentBlockMeta,
}

#[derive(Deserialize)]
struct ClaudeContentBlockMeta {
    #[serde(rename = "type")]
    block_type: String,
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
    // TODO(gemini-phase4, #1659): `GeminiStreamPart` does not have a `function_call` field.
    // When Gemini streams a tool call via SSE, `functionCall` chunks in `parts` are silently
    // dropped here. `chat_with_tools()` currently uses the non-streaming endpoint, so this is
    // safe. If SSE streaming tool use is added (Phase 4 of epic #1592), extend
    // `GeminiStreamPart` with `function_call: Option<GeminiFunctionCall>` and handle it here.
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
        let mut state = ClaudeSseState::default();
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let result = parse_claude_sse_event(&mut state, data, "content_block_delta");
        let chunk = result.unwrap().unwrap();
        assert!(matches!(chunk, StreamChunk::Content(s) if s == "Hello"));
    }

    #[test]
    fn claude_parse_empty_text_delta() {
        let mut state = ClaudeSseState::default();
        let data =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#;
        let result = parse_claude_sse_event(&mut state, data, "content_block_delta");
        assert!(result.is_none());
    }

    #[test]
    fn claude_parse_error_event() {
        let mut state = ClaudeSseState::default();
        let data = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let result = parse_claude_sse_event(&mut state, data, "error");
        let err = result.unwrap().unwrap_err();
        assert!(err.to_string().contains("overloaded_error"));
    }

    #[test]
    fn claude_parse_unknown_event_skipped() {
        let mut state = ClaudeSseState::default();
        let result = parse_claude_sse_event(&mut state, "{}", "ping");
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
        let mut state = ClaudeSseState::default();
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I need to think about this"}}"#;
        let result = parse_claude_sse_event(&mut state, data, "content_block_delta");
        let chunk = result.unwrap().unwrap();
        assert!(matches!(chunk, StreamChunk::Thinking(s) if s == "I need to think about this"));
    }

    #[test]
    fn claude_thinking_delta_empty_not_emitted() {
        let mut state = ClaudeSseState::default();
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#;
        let result = parse_claude_sse_event(&mut state, data, "content_block_delta");
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
        let mut state = ClaudeSseState::default();
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc123"}}"#;
        let result = parse_claude_sse_event(&mut state, data, "content_block_delta");
        assert!(result.is_none());
    }

    #[test]
    fn claude_compaction_block_start_sets_buf() {
        let mut state = ClaudeSseState::default();
        assert!(state.compaction_buf.is_none());
        let data = r#"{"type":"content_block_start","index":0,"content_block":{"type":"compaction","text":""}}"#;
        let result = parse_claude_sse_event(&mut state, data, "content_block_start");
        assert!(result.is_none());
        assert!(state.compaction_buf.is_some());
    }

    #[test]
    fn claude_non_compaction_block_start_leaves_buf_empty() {
        let mut state = ClaudeSseState::default();
        let data =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let result = parse_claude_sse_event(&mut state, data, "content_block_start");
        assert!(result.is_none());
        assert!(state.compaction_buf.is_none());
    }

    #[test]
    fn claude_compaction_delta_accumulated_into_buf() {
        let mut state = ClaudeSseState {
            compaction_buf: Some(String::new()),
        };
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Summary text"}}"#;
        let result = parse_claude_sse_event(&mut state, data, "content_block_delta");
        assert!(result.is_none());
        assert_eq!(state.compaction_buf.as_deref(), Some("Summary text"));
    }

    #[test]
    fn claude_compaction_delta_does_not_emit_content_chunk() {
        let mut state = ClaudeSseState {
            compaction_buf: Some("so far".to_owned()),
        };
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" more"}}"#;
        let result = parse_claude_sse_event(&mut state, data, "content_block_delta");
        // Must not emit a Content chunk while accumulating compaction.
        assert!(result.is_none());
        assert_eq!(state.compaction_buf.as_deref(), Some("so far more"));
    }

    #[test]
    fn claude_compaction_stop_emits_compaction_chunk() {
        let mut state = ClaudeSseState {
            compaction_buf: Some("Final summary".to_owned()),
        };
        let result = parse_claude_sse_event(&mut state, "{}", "content_block_stop");
        let chunk = result.unwrap().unwrap();
        assert!(
            matches!(chunk, StreamChunk::Compaction(s) if s == "Final summary"),
            "expected Compaction chunk with full summary"
        );
        assert!(state.compaction_buf.is_none());
    }

    #[test]
    fn claude_stop_without_compaction_buf_returns_none() {
        let mut state = ClaudeSseState::default();
        let result = parse_claude_sse_event(&mut state, "{}", "content_block_stop");
        assert!(result.is_none());
    }

    #[test]
    fn claude_compaction_buf_capped_at_32kib() {
        let mut state = ClaudeSseState {
            compaction_buf: Some("x".repeat(32 * 1024 - 1)),
        };
        // Two-byte push that would exceed cap.
        let data =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ab"}}"#;
        parse_claude_sse_event(&mut state, data, "content_block_delta");
        let buf = state.compaction_buf.as_ref().unwrap();
        assert!(buf.len() <= 32 * 1024, "buffer must not exceed 32 KiB");
    }

    #[test]
    fn claude_full_compaction_sequence() {
        let mut state = ClaudeSseState::default();
        // 1. block_start with compaction type
        let start = r#"{"type":"content_block_start","index":0,"content_block":{"type":"compaction","text":""}}"#;
        assert!(parse_claude_sse_event(&mut state, start, "content_block_start").is_none());
        // 2. delta
        let delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Summarized context"}}"#;
        assert!(parse_claude_sse_event(&mut state, delta, "content_block_delta").is_none());
        // 3. stop
        let result = parse_claude_sse_event(&mut state, "{}", "content_block_stop");
        let chunk = result.unwrap().unwrap();
        assert!(matches!(chunk, StreamChunk::Compaction(s) if s == "Summarized context"));
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
