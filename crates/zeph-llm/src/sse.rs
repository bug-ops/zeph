// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use eventsource_stream::Eventsource;
use serde::Deserialize;
use tokio_stream::StreamExt;

use crate::error::LlmError;
use crate::provider::{ChatStream, StreamChunk, ThinkingBlock, ToolUseRequest};

/// An internal SSE event emitted by `claude_sse_to_tool_stream`.
///
/// Never exposed as a `StreamChunk` downstream — the drainer consumes these and
/// surfaces only `StreamChunk` variants, keeping `StreamChunk` exhaustiveness stable
/// (critic M3). `pub` so `zeph-core` can use the drainer without feature-gating.
#[derive(Debug)]
pub enum ToolSseEvent {
    /// Tool block opened: id and name are now known, before any `InputJsonDelta` for this index.
    ///
    /// Emitted at `content_block_start` so the drainer can populate `tool_meta` immediately
    /// and avoid the timing gap where deltas arrive before the `ToolCallComplete` stop event.
    ToolBlockStart {
        index: usize,
        id: String,
        name: String,
    },
    /// Incremental JSON fragment for the tool at `index`.
    InputJsonDelta { index: usize, delta: String },
    /// A tool-use block is fully received: carries the complete accumulated JSON.
    ToolCallComplete {
        index: usize,
        id: String,
        name: String,
        full_json: String,
    },
    /// Accumulated thinking text chunk (pass-through for `StreamChunk::Thinking`).
    ThinkingChunk(String),
    /// Completed thinking block (thinking text + signature) ready for `ChatResponse`.
    ThinkingBlockDone(ThinkingBlock),
    /// Regular assistant text chunk.
    ContentChunk(String),
    /// Server-side compaction summary.
    Compaction(String),
    /// Parse error.
    Error(LlmError),
}

/// A stream of internal tool SSE events, used by `SpeculativeStreamDrainer`.
pub type ToolSseStream = std::pin::Pin<Box<dyn tokio_stream::Stream<Item = ToolSseEvent> + Send>>;

/// State machine for accumulating multi-event Claude SSE blocks.
#[derive(Default)]
struct ClaudeSseState {
    /// When `Some`, we are accumulating a compaction block. Holds the summary text so far.
    compaction_buf: Option<String>,
    /// Tracks the current in-flight tool-use block: `(index, id, name, accumulated_json)`.
    tool_block: Option<(usize, String, String, String)>,
    /// Tracks the current in-flight thinking block: `(accumulated_thinking, accumulated_signature)`.
    thinking_block: Option<(String, String)>,
    /// Index of the current content block (from `content_block_start.index`).
    current_block_index: usize,
    /// `true` when the current block is a thinking block.
    in_thinking_block: bool,
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
                    for chunk in parse_claude_sse_events(&mut state, &ev.data, &ev.event) {
                        yield chunk;
                    }
                }
                Err(e) => yield Err(LlmError::SseParse(e.to_string())),
            }
        }
    };
    Box::pin(s)
}

/// Convert a Claude streaming tool-use response into a [`ToolSseStream`].
///
/// Emits `ToolSseEvent` variants so `SpeculativeStreamDrainer` can intercept
/// `InputJsonDelta` events for early speculative dispatch, while passing other
/// events through to reconstruct a `ChatResponse` at the end.
pub(crate) fn claude_sse_to_tool_stream(response: reqwest::Response) -> ToolSseStream {
    let event_stream = response.bytes_stream().eventsource();
    let s = async_stream::stream! {
        let mut state = ClaudeSseState::default();
        let mut pinned = std::pin::pin!(event_stream);
        while let Some(event) = pinned.next().await {
            match event {
                Ok(ev) => {
                    for tool_ev in parse_claude_tool_sse_events(&mut state, &ev.data, &ev.event) {
                        yield tool_ev;
                    }
                }
                Err(e) => yield ToolSseEvent::Error(LlmError::SseParse(e.to_string())),
            }
        }
    };
    Box::pin(s)
}

/// Convert a Gemini streaming response into a `ChatStream`.
pub(crate) fn gemini_sse_to_stream(response: reqwest::Response) -> ChatStream {
    stateless_sse_to_stream(response, parse_gemini_sse_event)
}

/// Convert an `OpenAI` streaming response into a `ChatStream`.
pub(crate) fn openai_sse_to_stream(response: reqwest::Response) -> ChatStream {
    stateless_sse_to_stream(response, parse_openai_sse_event)
}

/// Shared helper for stateless SSE providers: applies `parse_fn` to each event and
/// wraps parse errors in `LlmError::SseParse`.
fn stateless_sse_to_stream(
    response: reqwest::Response,
    parse_fn: fn(&str) -> Option<Result<StreamChunk, LlmError>>,
) -> ChatStream {
    let event_stream = response.bytes_stream().eventsource();
    let mapped = event_stream.filter_map(move |event| match event {
        Ok(event) => parse_fn(&event.data),
        Err(e) => Some(Err(LlmError::SseParse(e.to_string()))),
    });
    Box::pin(mapped)
}

/// Parse a single Claude SSE event for the text-streaming path.
///
/// Returns zero or more `StreamChunk` results. Most events produce at most one chunk,
/// but having a slice return avoids Option-chaining at call sites.
fn parse_claude_sse_events(
    state: &mut ClaudeSseState,
    data: &str,
    event_type: &str,
) -> Vec<Result<StreamChunk, LlmError>> {
    match event_type {
        "content_block_start" => {
            match serde_json::from_str::<ClaudeContentBlockStart>(data) {
                Ok(ev) if ev.content_block.block_type == "compaction" => {
                    tracing::debug!("Claude compaction block started");
                    state.compaction_buf = Some(String::new());
                }
                _ => {}
            }
            vec![]
        }
        "content_block_stop" => {
            if let Some(summary) = state.compaction_buf.take() {
                tracing::info!(
                    summary_len = summary.len(),
                    "Claude server-side compaction block completed in stream"
                );
                return vec![Ok(StreamChunk::Compaction(summary))];
            }
            vec![]
        }
        "content_block_delta" => match serde_json::from_str::<ClaudeStreamEvent>(data) {
            Ok(event) => {
                if let Some(delta) = event.delta {
                    match delta.delta_type.as_str() {
                        "text_delta" if !delta.text.is_empty() => {
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
                                return vec![];
                            }
                            return vec![Ok(StreamChunk::Content(delta.text))];
                        }
                        "thinking_delta" if !delta.thinking.is_empty() => {
                            return vec![Ok(StreamChunk::Thinking(delta.thinking))];
                        }
                        "signature_delta" => {
                            tracing::debug!("Claude signature_delta (not emitted to stream)");
                        }
                        _ => {}
                    }
                }
                vec![]
            }
            Err(e) => vec![Err(LlmError::SseParse(format!(
                "failed to parse SSE data: {e}"
            )))],
        },
        "error" => match serde_json::from_str::<ClaudeStreamEvent>(data) {
            Ok(event) => {
                if let Some(err) = event.error {
                    vec![Err(LlmError::SseParse(format!(
                        "Claude stream error ({}): {}",
                        err.error_type, err.message
                    )))]
                } else {
                    vec![Err(LlmError::SseParse(format!(
                        "Claude stream error: {data}"
                    )))]
                }
            }
            Err(_) => vec![Err(LlmError::SseParse(format!(
                "Claude stream error: {data}"
            )))],
        },
        _ => vec![],
    }
}

/// Parse a single Claude SSE event for the tool-use streaming path.
///
/// Emits `ToolSseEvent` variants so `SpeculativeStreamDrainer` can handle
/// `InputJsonDelta` incrementally while accumulating thinking blocks for the
/// final `ChatResponse` (critic H2).
fn parse_claude_tool_sse_events(
    state: &mut ClaudeSseState,
    data: &str,
    event_type: &str,
) -> Vec<ToolSseEvent> {
    match event_type {
        "content_block_start" => {
            let Ok(ev) = serde_json::from_str::<ClaudeContentBlockStartFull>(data) else {
                return vec![];
            };
            state.current_block_index = ev.index;
            state.in_thinking_block = false;
            match ev.content_block.block_type.as_str() {
                "compaction" => {
                    state.compaction_buf = Some(String::new());
                    vec![]
                }
                "tool_use" => {
                    let id = ev.content_block.id.unwrap_or_default();
                    let name = ev.content_block.name.unwrap_or_default();
                    // Store metadata so InputJsonDelta can accumulate into the right buffer.
                    state.tool_block = Some((ev.index, id.clone(), name.clone(), String::new()));
                    // Emit ToolBlockStart immediately so the drainer knows id+name before any
                    // InputJsonDelta arrives for this index (fixes the tool_meta timing gap).
                    vec![ToolSseEvent::ToolBlockStart {
                        index: ev.index,
                        id,
                        name,
                    }]
                }
                "thinking" => {
                    state.in_thinking_block = true;
                    state.thinking_block = Some((String::new(), String::new()));
                    vec![]
                }
                _ => vec![],
            }
        }
        "content_block_stop" => {
            let mut events = vec![];
            if let Some(summary) = state.compaction_buf.take() {
                tracing::info!(
                    summary_len = summary.len(),
                    "Claude server-side compaction block completed in tool stream"
                );
                events.push(ToolSseEvent::Compaction(summary));
            }
            if let Some((index, id, name, json)) = state.tool_block.take() {
                events.push(ToolSseEvent::ToolCallComplete {
                    index,
                    id,
                    name,
                    full_json: json,
                });
            }
            if let Some((thinking, signature)) = state.thinking_block.take()
                && (!thinking.is_empty() || !signature.is_empty())
            {
                events.push(ToolSseEvent::ThinkingBlockDone(ThinkingBlock::Thinking {
                    thinking,
                    signature,
                }));
            }
            state.in_thinking_block = false;
            events
        }
        "content_block_delta" => parse_tool_delta_event(state, data),
        "error" => parse_sse_error_event(data),
        _ => vec![],
    }
}

fn parse_tool_delta_event(state: &mut ClaudeSseState, data: &str) -> Vec<ToolSseEvent> {
    let Ok(event) = serde_json::from_str::<ClaudeStreamEvent>(data) else {
        return vec![ToolSseEvent::Error(LlmError::SseParse(format!(
            "failed to parse SSE data: {data}"
        )))];
    };
    let Some(delta) = event.delta else {
        return vec![];
    };
    match delta.delta_type.as_str() {
        "text_delta" if !delta.text.is_empty() => {
            if let Some(ref mut buf) = state.compaction_buf {
                const MAX_COMPACTION_BUF: usize = 32 * 1024;
                let remaining = MAX_COMPACTION_BUF.saturating_sub(buf.len());
                if remaining > 0 {
                    let to_append = &delta.text[..delta.text.len().min(remaining)];
                    buf.push_str(to_append);
                } else {
                    tracing::warn!("compaction buffer exceeded 32 KiB cap; discarding excess");
                }
                return vec![];
            }
            vec![ToolSseEvent::ContentChunk(delta.text)]
        }
        "input_json_delta" if !delta.partial_json.is_empty() => {
            if let Some((_, _, _, ref mut json)) = state.tool_block {
                json.push_str(&delta.partial_json);
            }
            let index = state.current_block_index;
            vec![ToolSseEvent::InputJsonDelta {
                index,
                delta: delta.partial_json,
            }]
        }
        "thinking_delta" if !delta.thinking.is_empty() => {
            if let Some((ref mut t, _)) = state.thinking_block {
                t.push_str(&delta.thinking);
            }
            vec![ToolSseEvent::ThinkingChunk(delta.thinking)]
        }
        "signature_delta" if !delta.signature.is_empty() => {
            if let Some((_, ref mut s)) = state.thinking_block {
                s.push_str(&delta.signature);
            }
            vec![]
        }
        _ => vec![],
    }
}

fn parse_sse_error_event(data: &str) -> Vec<ToolSseEvent> {
    match serde_json::from_str::<ClaudeStreamEvent>(data) {
        Ok(event) => {
            if let Some(err) = event.error {
                vec![ToolSseEvent::Error(LlmError::SseParse(format!(
                    "Claude stream error ({}): {}",
                    err.error_type, err.message
                )))]
            } else {
                vec![ToolSseEvent::Error(LlmError::SseParse(format!(
                    "Claude stream error: {data}"
                )))]
            }
        }
        Err(_) => vec![ToolSseEvent::Error(LlmError::SseParse(format!(
            "Claude stream error: {data}"
        )))],
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

/// Used for `content_block_start` events in the text-streaming path (type only).
#[derive(Deserialize)]
struct ClaudeContentBlockStart {
    content_block: ClaudeContentBlockMeta,
}

#[derive(Deserialize)]
struct ClaudeContentBlockMeta {
    #[serde(rename = "type")]
    block_type: String,
}

/// Used for `content_block_start` events in the tool-streaming path (full metadata).
#[derive(Deserialize)]
struct ClaudeContentBlockStartFull {
    #[serde(default)]
    index: usize,
    content_block: ClaudeContentBlockMetaFull,
}

/// Full metadata for a content block (type + optional tool-use fields).
#[derive(Deserialize)]
struct ClaudeContentBlockMetaFull {
    #[serde(rename = "type")]
    block_type: String,
    /// Present when `type == "tool_use"`.
    #[serde(default)]
    id: Option<String>,
    /// Present when `type == "tool_use"`.
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeDelta {
    #[serde(rename = "type")]
    delta_type: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    thinking: String,
    /// Present in `input_json_delta` events for tool-use streaming.
    #[serde(default, rename = "partial_json")]
    partial_json: String,
    /// Present in `signature_delta` events for thinking blocks.
    #[serde(default)]
    signature: String,
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

    // Collect thinking, content, and function call parts separately.
    // NOTE: Gemini delivers functionCall as a complete object per SSE event (not streamed
    // incrementally like OpenAI). If this behavior changes, an accumulator similar to
    // ClaudeSseState would be needed.
    let mut thinking = String::new();
    let mut content = String::new();
    let mut tool_calls: Vec<ToolUseRequest> = Vec::new();
    for part in parts {
        if let Some(ref fc) = part.function_call {
            tool_calls.push(ToolUseRequest {
                id: uuid::Uuid::new_v4().to_string(),
                name: fc.name.clone().into(),
                input: fc
                    .args
                    .clone()
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::default())),
            });
        } else if let Some(text) = part.text.as_deref()
            && !text.is_empty()
        {
            if part.thought == Some(true) {
                thinking.push_str(text);
            } else {
                content.push_str(text);
            }
        }
    }

    // Tool calls take priority. Text alongside tool calls is discarded (matches non-streaming
    // behavior in gemini.rs where ChatResponse::ToolUse is returned instead of Text).
    if !tool_calls.is_empty() {
        if !content.is_empty() {
            tracing::debug!(
                dropped_text_len = content.len(),
                "text dropped in favor of tool calls in Gemini SSE event"
            );
        }
        return Some(Ok(StreamChunk::ToolUse(tool_calls)));
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

/// SSE-specific function call part. Mirrors `GeminiFunctionCall` in `gemini.rs` but kept
/// separate to allow independent serde evolution for the streaming path.
#[derive(Deserialize)]
struct GeminiStreamFunctionCall {
    name: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct GeminiStreamPart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
    #[serde(default, rename = "functionCall")]
    function_call: Option<GeminiStreamFunctionCall>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_parse_text_delta() {
        let mut state = ClaudeSseState::default();
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let mut results = parse_claude_sse_events(&mut state, data, "content_block_delta");
        assert_eq!(results.len(), 1);
        let chunk = results.remove(0).unwrap();
        assert!(matches!(chunk, StreamChunk::Content(s) if s == "Hello"));
    }

    #[test]
    fn claude_parse_empty_text_delta() {
        let mut state = ClaudeSseState::default();
        let data =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#;
        let results = parse_claude_sse_events(&mut state, data, "content_block_delta");
        assert!(results.is_empty());
    }

    #[test]
    fn claude_parse_error_event() {
        let mut state = ClaudeSseState::default();
        let data = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let mut results = parse_claude_sse_events(&mut state, data, "error");
        assert_eq!(results.len(), 1);
        let err = results.remove(0).unwrap_err();
        assert!(err.to_string().contains("overloaded_error"));
    }

    #[test]
    fn claude_parse_unknown_event_skipped() {
        let mut state = ClaudeSseState::default();
        let results = parse_claude_sse_events(&mut state, "{}", "ping");
        assert!(results.is_empty());
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
        let mut results = parse_claude_sse_events(&mut state, data, "content_block_delta");
        assert_eq!(results.len(), 1);
        let chunk = results.remove(0).unwrap();
        assert!(matches!(chunk, StreamChunk::Thinking(s) if s == "I need to think about this"));
    }

    #[test]
    fn claude_thinking_delta_empty_not_emitted() {
        let mut state = ClaudeSseState::default();
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#;
        let results = parse_claude_sse_events(&mut state, data, "content_block_delta");
        assert!(results.is_empty());
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
        let results = parse_claude_sse_events(&mut state, data, "content_block_delta");
        assert!(results.is_empty());
    }

    #[test]
    fn claude_compaction_block_start_sets_buf() {
        let mut state = ClaudeSseState::default();
        assert!(state.compaction_buf.is_none());
        let data = r#"{"type":"content_block_start","index":0,"content_block":{"type":"compaction","text":""}}"#;
        let results = parse_claude_sse_events(&mut state, data, "content_block_start");
        assert!(results.is_empty());
        assert!(state.compaction_buf.is_some());
    }

    #[test]
    fn claude_non_compaction_block_start_leaves_buf_empty() {
        let mut state = ClaudeSseState::default();
        let data =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let results = parse_claude_sse_events(&mut state, data, "content_block_start");
        assert!(results.is_empty());
        assert!(state.compaction_buf.is_none());
    }

    #[test]
    fn claude_compaction_delta_accumulated_into_buf() {
        let mut state = ClaudeSseState {
            compaction_buf: Some(String::new()),
            ..Default::default()
        };
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Summary text"}}"#;
        let results = parse_claude_sse_events(&mut state, data, "content_block_delta");
        assert!(results.is_empty());
        assert_eq!(state.compaction_buf.as_deref(), Some("Summary text"));
    }

    #[test]
    fn claude_compaction_delta_does_not_emit_content_chunk() {
        let mut state = ClaudeSseState {
            compaction_buf: Some("so far".to_owned()),
            ..Default::default()
        };
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" more"}}"#;
        let results = parse_claude_sse_events(&mut state, data, "content_block_delta");
        // Must not emit a Content chunk while accumulating compaction.
        assert!(results.is_empty());
        assert_eq!(state.compaction_buf.as_deref(), Some("so far more"));
    }

    #[test]
    fn claude_compaction_stop_emits_compaction_chunk() {
        let mut state = ClaudeSseState {
            compaction_buf: Some("Final summary".to_owned()),
            ..Default::default()
        };
        let mut results = parse_claude_sse_events(&mut state, "{}", "content_block_stop");
        assert_eq!(results.len(), 1);
        let chunk = results.remove(0).unwrap();
        assert!(
            matches!(chunk, StreamChunk::Compaction(s) if s == "Final summary"),
            "expected Compaction chunk with full summary"
        );
        assert!(state.compaction_buf.is_none());
    }

    #[test]
    fn claude_stop_without_compaction_buf_returns_none() {
        let mut state = ClaudeSseState::default();
        let results = parse_claude_sse_events(&mut state, "{}", "content_block_stop");
        assert!(results.is_empty());
    }

    #[test]
    fn claude_compaction_buf_capped_at_32kib() {
        let mut state = ClaudeSseState {
            compaction_buf: Some("x".repeat(32 * 1024 - 1)),
            ..Default::default()
        };
        // Two-byte push that would exceed cap.
        let data =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ab"}}"#;
        parse_claude_sse_events(&mut state, data, "content_block_delta");
        let buf = state.compaction_buf.as_ref().unwrap();
        assert!(buf.len() <= 32 * 1024, "buffer must not exceed 32 KiB");
    }

    #[test]
    fn claude_full_compaction_sequence() {
        let mut state = ClaudeSseState::default();
        // 1. block_start with compaction type
        let start = r#"{"type":"content_block_start","index":0,"content_block":{"type":"compaction","text":""}}"#;
        assert!(parse_claude_sse_events(&mut state, start, "content_block_start").is_empty());
        // 2. delta
        let delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Summarized context"}}"#;
        assert!(parse_claude_sse_events(&mut state, delta, "content_block_delta").is_empty());
        // 3. stop
        let mut results = parse_claude_sse_events(&mut state, "{}", "content_block_stop");
        assert_eq!(results.len(), 1);
        let chunk = results.remove(0).unwrap();
        assert!(matches!(chunk, StreamChunk::Compaction(s) if s == "Summarized context"));
    }

    #[test]
    fn claude_tool_sse_input_json_delta_emitted() {
        let mut state = ClaudeSseState {
            tool_block: Some((0, "toolu_01".into(), "bash".into(), String::new())),
            current_block_index: 0,
            ..Default::default()
        };
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"ls"}}"#;
        let events = parse_claude_tool_sse_events(&mut state, data, "content_block_delta");
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], ToolSseEvent::InputJsonDelta { index: 0, delta } if delta == r#"{"cmd":"ls"#)
        );
    }

    #[test]
    fn claude_tool_sse_tool_call_complete_on_stop() {
        let mut state = ClaudeSseState {
            tool_block: Some((
                1,
                "toolu_02".into(),
                "read_file".into(),
                r#"{"path":"a.rs"}"#.into(),
            )),
            current_block_index: 1,
            ..Default::default()
        };
        let events = parse_claude_tool_sse_events(&mut state, "{}", "content_block_stop");
        assert!(state.tool_block.is_none());
        assert!(
            events.iter().any(|e| matches!(e, ToolSseEvent::ToolCallComplete { index: 1, name, .. } if name == "read_file"))
        );
    }

    #[test]
    fn claude_tool_sse_thinking_block_done_on_stop() {
        let mut state = ClaudeSseState {
            thinking_block: Some(("think".into(), "sig".into())),
            in_thinking_block: true,
            ..Default::default()
        };
        let events = parse_claude_tool_sse_events(&mut state, "{}", "content_block_stop");
        assert!(state.thinking_block.is_none());
        assert!(
            events.iter().any(|e| matches!(e, ToolSseEvent::ThinkingBlockDone(ThinkingBlock::Thinking { thinking, .. }) if thinking == "think"))
        );
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

    #[test]
    fn gemini_parse_single_function_call() {
        let data = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_weather","args":{"city":"Paris"}}}]}}]}"#;
        let result = parse_gemini_sse_event(data);
        let chunk = result.unwrap().unwrap();
        match chunk {
            StreamChunk::ToolUse(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "get_weather");
                assert_eq!(calls[0].input["city"], "Paris");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn gemini_parse_multiple_function_calls() {
        let data = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"tool_a","args":{}}},{"functionCall":{"name":"tool_b","args":{"x":1}}}]}}]}"#;
        let result = parse_gemini_sse_event(data);
        let chunk = result.unwrap().unwrap();
        match chunk {
            StreamChunk::ToolUse(calls) => {
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].name, "tool_a");
                assert_eq!(calls[1].name, "tool_b");
                assert_eq!(calls[1].input["x"], 1);
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn gemini_parse_function_call_no_args() {
        // Missing `args` field — should default to empty object.
        let data = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"ping"}}]}}]}"#;
        let result = parse_gemini_sse_event(data);
        let chunk = result.unwrap().unwrap();
        match chunk {
            StreamChunk::ToolUse(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "ping");
                assert!(calls[0].input.is_object());
                assert_eq!(calls[0].input.as_object().unwrap().len(), 0);
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn gemini_parse_function_call_null_args() {
        // Explicit `null` args — should also default to empty object.
        let data = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"ping","args":null}}]}}]}"#;
        let result = parse_gemini_sse_event(data);
        let chunk = result.unwrap().unwrap();
        match chunk {
            StreamChunk::ToolUse(calls) => {
                assert_eq!(calls.len(), 1);
                assert!(calls[0].input.is_object());
                assert_eq!(calls[0].input.as_object().unwrap().len(), 0);
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn gemini_parse_mixed_text_and_function_call() {
        // Text alongside functionCall — tool call takes priority, text is dropped.
        let data = r#"{"candidates":[{"content":{"parts":[{"text":"Let me look that up"},{"functionCall":{"name":"search","args":{"q":"rust"}}}]}}]}"#;
        let result = parse_gemini_sse_event(data);
        let chunk = result.unwrap().unwrap();
        match chunk {
            StreamChunk::ToolUse(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "search");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn gemini_parse_function_call_with_thinking() {
        // Thinking + functionCall — tool call takes priority.
        let data = r#"{"candidates":[{"content":{"parts":[{"text":"reasoning","thought":true},{"functionCall":{"name":"calc","args":{}}}]}}]}"#;
        let result = parse_gemini_sse_event(data);
        let chunk = result.unwrap().unwrap();
        match chunk {
            StreamChunk::ToolUse(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "calc");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn gemini_parse_function_call_empty_name() {
        // Empty name is passed through — caller is responsible for validation.
        let data =
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"","args":{}}}]}}]}"#;
        let result = parse_gemini_sse_event(data);
        let chunk = result.unwrap().unwrap();
        match chunk {
            StreamChunk::ToolUse(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn gemini_parse_text_only_unaffected() {
        // Regression: pure text events must still return Content.
        let data = r#"{"candidates":[{"content":{"parts":[{"text":"Hello world"}]}}]}"#;
        let result = parse_gemini_sse_event(data);
        let chunk = result.unwrap().unwrap();
        assert!(matches!(chunk, StreamChunk::Content(s) if s == "Hello world"));
    }
}
