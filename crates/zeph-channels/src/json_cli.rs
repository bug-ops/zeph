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
    /// True iff at least one `ResponseChunk` has been emitted since the last
    /// `ResponseEnd`. `ResponseEnd` must never be emitted when this is `false`.
    /// Both `send()` and `send_chunk()` set this to `true`; `flush_chunks()` resets it to `false`.
    pending_chunks: bool,
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

        Self {
            sink,
            rx,
            auto,
            pending_chunks: false,
        }
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
                        is_guest_context: false,
                        is_from_bot: false,
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
                    is_guest_context: false,
                    is_from_bot: false,
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
        self.pending_chunks = true;
        Ok(())
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        self.sink.emit(&JsonEvent::ResponseChunk { text: chunk });
        self.pending_chunks = true;
        Ok(())
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        if self.pending_chunks {
            self.sink.emit(&JsonEvent::ResponseEnd);
            self.pending_chunks = false;
        }
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
        // Elicitation is not supported in JSON mode; decline quietly to avoid log spam.
        Ok(ElicitationResponse::Declined)
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

    async fn send_diff(
        &mut self,
        _diff: DiffData,
        _tool_call_id: &str,
    ) -> Result<(), ChannelError> {
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
    use std::sync::{Arc, Mutex};

    use zeph_core::json_event_sink::JsonEventSink;

    use super::*;

    struct BufWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for BufWriter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Returns a `(sink, read_output)` pair. `read_output()` returns captured JSONL lines.
    fn make_test_sink() -> (Arc<JsonEventSink>, impl Fn() -> Vec<String>) {
        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let buf_read = Arc::clone(&buf);

        let sink = Arc::new(JsonEventSink::with_writer(BufWriter(buf)));
        let read = move || {
            let data = buf_read.lock().unwrap();
            String::from_utf8(data.clone())
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        (sink, read)
    }

    fn event_field<'a>(line: &'a str, key: &str) -> &'a str {
        // minimal parse: find `"key":"value"` in the JSONL line
        let needle = format!("\"{key}\":\"");
        line.find(&needle).map_or("", |i| {
            let rest = &line[i + needle.len()..];
            &rest[..rest.find('"').unwrap_or(rest.len())]
        })
    }

    #[tokio::test]
    async fn flush_chunks_is_noop_without_chunks() {
        let (sink, read) = make_test_sink();
        let mut ch = JsonCliChannel::new(Arc::clone(&sink), false);
        ch.flush_chunks().await.unwrap();
        assert!(
            read().is_empty(),
            "flush_chunks must not emit when no chunks were sent"
        );
    }

    #[tokio::test]
    async fn flush_chunks_emits_end_after_chunk() {
        let (sink, read) = make_test_sink();
        let mut ch = JsonCliChannel::new(Arc::clone(&sink), false);
        ch.send_chunk("hello").await.unwrap();
        ch.flush_chunks().await.unwrap();
        let lines = read();
        assert_eq!(lines.len(), 2);
        assert_eq!(event_field(&lines[0], "event"), "response_chunk");
        assert_eq!(event_field(&lines[1], "event"), "response_end");
    }

    #[tokio::test]
    async fn send_sets_pending_and_flush_emits_end() {
        // send() only emits ResponseChunk; flush_chunks() is the sole emitter of ResponseEnd.
        let (sink, read) = make_test_sink();
        let mut ch = JsonCliChannel::new(Arc::clone(&sink), false);
        ch.send_chunk("a").await.unwrap();
        ch.send("b").await.unwrap();
        // No ResponseEnd yet — pending_chunks is still true after send()
        assert!(
            !read()
                .iter()
                .any(|l| event_field(l, "event") == "response_end"),
            "send() must not emit ResponseEnd"
        );
        ch.flush_chunks().await.unwrap();
        let lines = read();
        assert_eq!(
            lines
                .iter()
                .filter(|l| event_field(l, "event") == "response_end")
                .count(),
            1,
            "flush_chunks must emit exactly one ResponseEnd; got: {lines:?}"
        );
    }

    #[tokio::test]
    async fn send_after_send_chunk_then_flush_emits_single_end() {
        // send_chunk("a") + send("b") + flush => chunk(a), chunk(b), response_end.
        let (sink, read) = make_test_sink();
        let mut ch = JsonCliChannel::new(Arc::clone(&sink), false);
        ch.send_chunk("a").await.unwrap();
        ch.send("b").await.unwrap();
        ch.flush_chunks().await.unwrap();
        let lines = read();
        assert_eq!(
            lines.len(),
            3,
            "expected chunk(a), chunk(b), response_end; got: {lines:?}"
        );
        assert_eq!(event_field(&lines[0], "event"), "response_chunk");
        assert_eq!(event_field(&lines[1], "event"), "response_chunk");
        assert_eq!(event_field(&lines[2], "event"), "response_end");
    }

    #[tokio::test]
    async fn two_sequential_sends_with_flush_emit_two_ends() {
        // Each send+flush pair emits exactly one ResponseEnd.
        let (sink, read) = make_test_sink();
        let mut ch = JsonCliChannel::new(Arc::clone(&sink), false);
        ch.send("first").await.unwrap();
        ch.flush_chunks().await.unwrap();
        ch.send("second").await.unwrap();
        ch.flush_chunks().await.unwrap();
        let lines = read();
        // chunk, end, chunk, end
        assert_eq!(lines.len(), 4);
        assert_eq!(event_field(&lines[1], "event"), "response_end");
        assert_eq!(event_field(&lines[3], "event"), "response_end");
    }

    #[tokio::test]
    async fn send_status_ok() {
        let (sink, _read) = make_test_sink();
        let mut ch = JsonCliChannel::new(Arc::clone(&sink), false);
        assert!(ch.send_status("working…").await.is_ok());
    }

    #[tokio::test]
    async fn no_ops_do_not_error() {
        use zeph_core::channel::{ToolOutputEvent, ToolStartEvent};
        let (sink, _read) = make_test_sink();
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
        let (sink, _) = make_test_sink();
        let ch = JsonCliChannel::new(sink, false);
        assert!(ch.supports_exit());
    }

    #[tokio::test]
    async fn send_then_marker_chunk_then_flush_emits_single_end() {
        // Regression for #3243: send() + send_chunk(marker) + flush_chunks() must emit exactly
        // one response_end. Previously send() emitted ResponseEnd, then flush_chunks() emitted
        // a second one when MARCH self-check appended a flag_marker chunk.
        let (sink, read) = make_test_sink();
        let mut ch = JsonCliChannel::new(Arc::clone(&sink), false);
        ch.send("The answer is 42.").await.unwrap();
        ch.send_chunk(" [flag]").await.unwrap();
        ch.flush_chunks().await.unwrap();
        let lines = read();
        let end_count = lines
            .iter()
            .filter(|l| event_field(l, "event") == "response_end")
            .count();
        assert_eq!(
            end_count, 1,
            "expected exactly one response_end; got {end_count} in: {lines:?}"
        );
    }

    #[tokio::test]
    async fn flag_marker_appended_via_send_chunk_has_single_end() {
        // Simulates: streaming response + self-check appends flag marker, then single flush.
        let (sink, read) = make_test_sink();
        let mut ch = JsonCliChannel::new(Arc::clone(&sink), false);
        ch.send_chunk("The answer is 42.").await.unwrap();
        ch.send_chunk(" [verify]").await.unwrap();
        ch.flush_chunks().await.unwrap();
        let lines = read();
        assert_eq!(lines.len(), 3, "expected 2 chunks + 1 end; got: {lines:?}");
        assert_eq!(event_field(&lines[0], "event"), "response_chunk");
        assert_eq!(event_field(&lines[1], "event"), "response_chunk");
        assert_eq!(event_field(&lines[2], "event"), "response_end");
    }

    #[test]
    fn try_recv_returns_none_when_no_input() {
        let (sink, _) = make_test_sink();
        let mut ch = JsonCliChannel::new(sink, false);
        assert!(ch.try_recv().is_none());
    }
}
