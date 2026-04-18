// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! JSON CLI channel: emits JSONL events to stdout, reads prompts from stdin.
//!
//! Used when `--json` is active. All log output is forced to stderr before
//! this channel is constructed, so stdout carries clean JSONL only.
//!
//! # Double-emission prevention
//!
//! `JsonEventLayer` (in `zeph-core`) is the canonical emitter for `tool_call`,
//! `tool_result`, and `cost` events. The corresponding channel methods
//! (`send_tool_start`, `send_tool_output`, `send_usage`) are intentionally no-ops here.

use std::io::{BufRead, BufReader, IsTerminal};
use std::sync::Arc;

use zeph_core::DiffData;
use zeph_core::channel::{
    Channel, ChannelError, ChannelMessage, ElicitationRequest, ElicitationResponse, StopHint,
    ToolOutputEvent, ToolStartEvent,
};
use zeph_core::json_event_sink::{JsonEvent, JsonEventSink};

/// CLI channel that emits structured JSON events to stdout.
///
/// Construct via [`JsonCliChannel::new`] and wrap in `AnyChannel::JsonCli`.
/// All assistive output goes through the shared [`JsonEventSink`]; only stdin
/// reading is internal to this channel.
#[derive(Debug)]
pub struct JsonCliChannel {
    sink: Arc<JsonEventSink>,
    /// Buffered stdin lines. `None` when stdin is a TTY (reads line-by-line).
    rx: tokio::sync::mpsc::Receiver<Option<String>>,
    /// Whether the caller should auto-approve confirmation prompts.
    auto: bool,
}

impl JsonCliChannel {
    /// Create a new channel.
    ///
    /// `auto` mirrors the `-y` / `--auto` flag: when `true`, `confirm()` always
    /// returns `true` without reading from stdin.
    #[must_use]
    pub fn new(sink: Arc<JsonEventSink>, auto: bool) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let is_tty = std::io::stdin().is_terminal();

        // Spawn a blocking reader thread so `recv` is cancel-safe.
        // TTY and piped stdin use the same line-by-line reader; no prompt is
        // printed in JSON mode.
        let _ = is_tty; // reserved for future TTY-specific behaviour
        std::thread::spawn(move || {
            let reader = BufReader::new(std::io::stdin().lock());
            for line in reader.lines() {
                if let Ok(l) = line {
                    if tx.blocking_send(Some(l)).is_err() {
                        break;
                    }
                } else {
                    let _ = tx.blocking_send(None);
                    break;
                }
            }
            // Signal EOF
            let _ = tx.blocking_send(None);
        });

        Self { sink, rx, auto }
    }
}

impl Channel for JsonCliChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        loop {
            match self.rx.recv().await {
                Some(Some(line)) => {
                    let trimmed = line.trim();
                    match trimmed {
                        "" => continue,
                        "exit" | "quit" | "/exit" | "/quit" => return Ok(None),
                        _ => {}
                    }
                    let text = trimmed.to_owned();
                    self.sink.emit(&JsonEvent::Query {
                        text: &text,
                        queue_len: 0,
                    });
                    return Ok(Some(ChannelMessage {
                        text,
                        attachments: Vec::new(),
                    }));
                }
                Some(None) | None => return Ok(None), // EOF
            }
        }
    }

    fn try_recv(&mut self) -> Option<ChannelMessage> {
        match self.rx.try_recv() {
            Ok(Some(line)) => {
                let trimmed = line.trim().to_owned();
                if trimmed.is_empty() {
                    return None;
                }
                self.sink.emit(&JsonEvent::Query {
                    text: &trimmed,
                    queue_len: 0,
                });
                Some(ChannelMessage {
                    text: trimmed,
                    attachments: Vec::new(),
                })
            }
            _ => None,
        }
    }

    fn supports_exit(&self) -> bool {
        true
    }

    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        self.sink.emit(&JsonEvent::ResponseChunk { text });
        self.sink.emit(&JsonEvent::ResponseEnd);
        Ok(())
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        self.sink.emit(&JsonEvent::ResponseChunk { text: chunk });
        Ok(())
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        self.sink.emit(&JsonEvent::ResponseEnd);
        Ok(())
    }

    async fn send_typing(&mut self) -> Result<(), ChannelError> {
        // No typing indicator in JSON mode.
        Ok(())
    }

    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        if self.auto {
            return Ok(true);
        }
        // Emit a status event and read one y/n line from the channel.
        self.sink.emit(&JsonEvent::Status {
            message: &format!("{prompt} (y/n)"),
        });
        match self.rx.recv().await {
            Some(Some(line)) => Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes")),
            _ => Ok(false),
        }
    }

    async fn elicit(
        &mut self,
        _request: ElicitationRequest,
    ) -> Result<ElicitationResponse, ChannelError> {
        // v1: elicitation over JSON is not supported.
        Err(ChannelError::Other(
            "elicitation not supported in --json mode".into(),
        ))
    }

    async fn send_status(&mut self, text: &str) -> Result<(), ChannelError> {
        self.sink.emit(&JsonEvent::Status { message: text });
        Ok(())
    }

    async fn send_queue_count(&mut self, count: usize) -> Result<(), ChannelError> {
        self.sink.emit(&JsonEvent::Status {
            message: &format!("queue: {count}"),
        });
        Ok(())
    }

    async fn send_diff(&mut self, _diff: DiffData) -> Result<(), ChannelError> {
        // v1: diffs are not emitted as JSON events.
        Ok(())
    }

    /// No-op: `JsonEventLayer` emits `tool_result` from its `after_tool` hook.
    /// Double-emission would corrupt the JSONL stream.
    async fn send_tool_output(&mut self, _event: ToolOutputEvent) -> Result<(), ChannelError> {
        Ok(())
    }

    async fn send_thinking_chunk(&mut self, _chunk: &str) -> Result<(), ChannelError> {
        // v1: thinking chunks are not emitted in JSON mode.
        Ok(())
    }

    async fn send_stop_hint(&mut self, hint: StopHint) -> Result<(), ChannelError> {
        self.sink.emit(&JsonEvent::Status {
            message: &format!("stop_hint: {hint:?}"),
        });
        Ok(())
    }

    /// No-op: `JsonEventLayer` emits `cost` from its `after_chat` hook.
    async fn send_usage(
        &mut self,
        _input_tokens: u64,
        _output_tokens: u64,
        _context_window: u64,
    ) -> Result<(), ChannelError> {
        Ok(())
    }

    /// No-op: `JsonEventLayer` emits `tool_call` from its `before_tool` hook.
    async fn send_tool_start(&mut self, _event: ToolStartEvent) -> Result<(), ChannelError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sink() -> Arc<JsonEventSink> {
        Arc::new(JsonEventSink::new())
    }

    #[tokio::test]
    async fn send_emits_chunk_and_end() {
        let sink = make_sink();
        let mut ch = JsonCliChannel::new(Arc::clone(&sink), false);
        assert!(ch.send("hello").await.is_ok());
    }

    #[tokio::test]
    async fn send_chunk_and_flush() {
        let sink = make_sink();
        let mut ch = JsonCliChannel::new(Arc::clone(&sink), false);
        assert!(ch.send_chunk("a").await.is_ok());
        assert!(ch.flush_chunks().await.is_ok());
    }

    #[tokio::test]
    async fn send_status_ok() {
        let sink = make_sink();
        let mut ch = JsonCliChannel::new(Arc::clone(&sink), false);
        assert!(ch.send_status("working…").await.is_ok());
    }

    #[tokio::test]
    async fn no_ops_do_not_error() {
        use zeph_core::channel::{ToolOutputEvent, ToolStartEvent};
        let sink = make_sink();
        let mut ch = JsonCliChannel::new(Arc::clone(&sink), false);
        assert!(ch.send_typing().await.is_ok());
        assert!(ch.send_thinking_chunk("...").await.is_ok());
        assert!(
            ch.send_tool_start(ToolStartEvent {
                tool_name: "shell".into(),
                tool_call_id: "x".into(),
                params: None,
                parent_tool_use_id: None,
                started_at: std::time::Instant::now(),
                speculative: false,
                sandbox_profile: None,
            })
            .await
            .is_ok()
        );
        assert!(
            ch.send_tool_output(ToolOutputEvent {
                tool_name: "shell".into(),
                display: "ok".into(),
                diff: None,
                filter_stats: None,
                kept_lines: None,
                locations: None,
                tool_call_id: "x".into(),
                is_error: false,
                terminal_id: None,
                parent_tool_use_id: None,
                raw_response: None,
                started_at: None,
            })
            .await
            .is_ok()
        );
        assert!(ch.send_usage(100, 50, 200_000).await.is_ok());
        assert!(ch.send_stop_hint(StopHint::MaxTokens).await.is_ok());
    }

    #[test]
    fn supports_exit_is_true() {
        let sink = make_sink();
        let ch = JsonCliChannel::new(sink, false);
        assert!(ch.supports_exit());
    }

    #[test]
    fn try_recv_returns_none_when_no_input() {
        let sink = make_sink();
        let mut ch = JsonCliChannel::new(sink, false);
        assert!(ch.try_recv().is_none());
    }
}
