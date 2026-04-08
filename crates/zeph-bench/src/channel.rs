// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::VecDeque;
use std::time::Instant;

use zeph_core::channel::{ChannelError, ChannelMessage, ToolOutputEvent};

/// A single captured agent response for one benchmark prompt.
#[derive(Debug, Clone)]
pub struct CapturedResponse {
    pub prompt_index: usize,
    pub text: String,
    pub elapsed: std::time::Duration,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub context_window: u64,
}

/// Headless channel that feeds pre-loaded prompts and captures agent responses.
///
/// Used by the bench runner to drive the agent loop without a real terminal or
/// network connection. `recv()` drains the prompt queue; `send()` / `flush_chunks()`
/// accumulate the response into `responses`.
pub struct BenchmarkChannel {
    prompts: VecDeque<String>,
    responses: Vec<CapturedResponse>,
    current_index: usize,
    total: usize,
    // Streaming chunk accumulation
    chunk_buffer: String,
    chunk_start: Option<Instant>,
    // Token usage for the current prompt (updated by send_usage)
    pending_input_tokens: u64,
    pending_output_tokens: u64,
    pending_context_window: u64,
}

impl BenchmarkChannel {
    /// Create a new channel from a list of prompt strings.
    #[must_use]
    pub fn new(prompts: Vec<String>) -> Self {
        let total = prompts.len();
        Self {
            prompts: VecDeque::from(prompts),
            responses: Vec::new(),
            current_index: 0,
            total,
            chunk_buffer: String::new(),
            chunk_start: None,
            pending_input_tokens: 0,
            pending_output_tokens: 0,
            pending_context_window: 0,
        }
    }

    /// Total number of prompts this channel was initialised with.
    #[must_use]
    pub fn total(&self) -> usize {
        self.total
    }

    /// Consume and return all captured responses.
    #[must_use]
    pub fn into_responses(self) -> Vec<CapturedResponse> {
        self.responses
    }

    /// Borrow the captured responses.
    #[must_use]
    pub fn responses(&self) -> &[CapturedResponse] {
        &self.responses
    }

    fn flush_chunk_buffer(&mut self) {
        if self.chunk_buffer.is_empty() {
            return;
        }
        let elapsed = self
            .chunk_start
            .map_or(std::time::Duration::ZERO, |s| s.elapsed());
        self.responses.push(CapturedResponse {
            prompt_index: self.current_index.saturating_sub(1),
            text: std::mem::take(&mut self.chunk_buffer),
            elapsed,
            input_tokens: self.pending_input_tokens,
            output_tokens: self.pending_output_tokens,
            context_window: self.pending_context_window,
        });
        self.chunk_start = None;
        self.pending_input_tokens = 0;
        self.pending_output_tokens = 0;
        self.pending_context_window = 0;
    }
}

impl zeph_core::channel::Channel for BenchmarkChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        match self.prompts.pop_front() {
            Some(text) => {
                self.current_index += 1;
                Ok(Some(ChannelMessage {
                    text,
                    attachments: vec![],
                }))
            }
            None => Ok(None),
        }
    }

    fn supports_exit(&self) -> bool {
        false
    }

    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        self.responses.push(CapturedResponse {
            prompt_index: self.current_index.saturating_sub(1),
            text: text.to_owned(),
            elapsed: std::time::Duration::ZERO,
            input_tokens: self.pending_input_tokens,
            output_tokens: self.pending_output_tokens,
            context_window: self.pending_context_window,
        });
        self.pending_input_tokens = 0;
        self.pending_output_tokens = 0;
        self.pending_context_window = 0;
        Ok(())
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        if self.chunk_start.is_none() {
            self.chunk_start = Some(Instant::now());
        }
        self.chunk_buffer.push_str(chunk);
        Ok(())
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        self.flush_chunk_buffer();
        Ok(())
    }

    async fn send_usage(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        context_window: u64,
    ) -> Result<(), ChannelError> {
        self.pending_input_tokens = input_tokens;
        self.pending_output_tokens = output_tokens;
        self.pending_context_window = context_window;
        Ok(())
    }

    // TODO(bench-runner): tool output is intentionally dropped here.
    // The default trait impl calls self.send(&formatted), which would push tool output
    // into responses and corrupt benchmark metrics. Override to no-op until Phase 2
    // when tool calls are captured separately.
    async fn send_tool_output(&mut self, _event: ToolOutputEvent<'_>) -> Result<(), ChannelError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use zeph_core::channel::{
        Channel, ElicitationField, ElicitationFieldType, ElicitationRequest, ElicitationResponse,
        ToolOutputEvent,
    };

    use super::*;

    #[tokio::test]
    async fn recv_drains_queue_and_returns_none_when_empty() {
        let mut ch = BenchmarkChannel::new(vec!["hello".into(), "world".into()]);
        let msg1 = ch.recv().await.unwrap().unwrap();
        assert_eq!(msg1.text, "hello");
        let msg2 = ch.recv().await.unwrap().unwrap();
        assert_eq!(msg2.text, "world");
        let msg3 = ch.recv().await.unwrap();
        assert!(msg3.is_none());
    }

    #[tokio::test]
    async fn send_accumulates_response() {
        let mut ch = BenchmarkChannel::new(vec!["prompt".into()]);
        let _ = ch.recv().await.unwrap();
        ch.send("response text").await.unwrap();
        assert_eq!(ch.responses().len(), 1);
        assert_eq!(ch.responses()[0].text, "response text");
    }

    #[tokio::test]
    async fn confirm_returns_true() {
        let mut ch = BenchmarkChannel::new(vec![]);
        let result = ch.confirm("delete?").await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn elicit_returns_declined() {
        let mut ch = BenchmarkChannel::new(vec![]);
        let req = ElicitationRequest {
            server_name: "test-server".into(),
            message: "provide input".into(),
            fields: vec![ElicitationField {
                name: "field".into(),
                description: None,
                field_type: ElicitationFieldType::String,
                required: true,
            }],
        };
        let result = ch.elicit(req).await.unwrap();
        assert!(matches!(result, ElicitationResponse::Declined));
    }

    #[tokio::test]
    async fn send_chunk_and_flush_captures_response() {
        let mut ch = BenchmarkChannel::new(vec!["p".into()]);
        let _ = ch.recv().await.unwrap();
        ch.send_chunk("part1").await.unwrap();
        ch.send_chunk(" part2").await.unwrap();
        ch.flush_chunks().await.unwrap();
        assert_eq!(ch.responses().len(), 1);
        assert_eq!(ch.responses()[0].text, "part1 part2");
    }

    #[tokio::test]
    async fn supports_exit_returns_false() {
        let ch = BenchmarkChannel::new(vec![]);
        assert!(!ch.supports_exit());
    }

    #[tokio::test]
    async fn send_usage_captured_on_send() {
        let mut ch = BenchmarkChannel::new(vec!["p".into()]);
        let _ = ch.recv().await.unwrap();
        ch.send_usage(10, 20, 128_000).await.unwrap();
        ch.send("answer").await.unwrap();
        let r = &ch.responses()[0];
        assert_eq!(r.input_tokens, 10);
        assert_eq!(r.output_tokens, 20);
        assert_eq!(r.context_window, 128_000);
    }

    #[tokio::test]
    async fn send_tool_output_does_not_add_to_responses() {
        let mut ch = BenchmarkChannel::new(vec!["p".into()]);
        let _ = ch.recv().await.unwrap();
        ch.send_tool_output(ToolOutputEvent {
            tool_name: "bash",
            body: "some tool output",
            diff: None,
            filter_stats: None,
            kept_lines: None,
            locations: None,
            tool_call_id: "tc-1",
            is_error: false,
            parent_tool_use_id: None,
            raw_response: None,
            started_at: None,
        })
        .await
        .unwrap();
        // Tool output must not be captured as a benchmark response.
        assert_eq!(ch.responses().len(), 0);
    }
}
