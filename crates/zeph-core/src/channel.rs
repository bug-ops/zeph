// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Typed error for channel operations.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    /// Underlying I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Channel closed (mpsc send/recv failure).
    #[error("channel closed")]
    ChannelClosed,

    /// Confirmation dialog cancelled.
    #[error("confirmation cancelled")]
    ConfirmCancelled,

    /// Catch-all for provider-specific errors.
    #[error("{0}")]
    Other(String),
}

/// Kind of binary attachment on an incoming message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Audio,
    Image,
    Video,
    File,
}

/// Binary attachment carried by a [`ChannelMessage`].
#[derive(Debug, Clone)]
pub struct Attachment {
    pub kind: AttachmentKind,
    pub data: Vec<u8>,
    pub filename: Option<String>,
}

/// Incoming message from a channel.
#[derive(Debug, Clone)]
pub struct ChannelMessage {
    pub text: String,
    pub attachments: Vec<Attachment>,
}

/// Bidirectional communication channel for the agent.
pub trait Channel: Send {
    /// Receive the next message. Returns `None` on EOF or shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn recv(&mut self)
    -> impl Future<Output = Result<Option<ChannelMessage>, ChannelError>> + Send;

    /// Non-blocking receive. Returns `None` if no message is immediately available.
    fn try_recv(&mut self) -> Option<ChannelMessage> {
        None
    }

    /// Send a text response.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send(&mut self, text: &str) -> impl Future<Output = Result<(), ChannelError>> + Send;

    /// Send a partial chunk of streaming response.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send_chunk(&mut self, chunk: &str) -> impl Future<Output = Result<(), ChannelError>> + Send;

    /// Flush any buffered chunks.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn flush_chunks(&mut self) -> impl Future<Output = Result<(), ChannelError>> + Send;

    /// Send a typing indicator. No-op by default.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send_typing(&mut self) -> impl Future<Output = Result<(), ChannelError>> + Send {
        async { Ok(()) }
    }

    /// Send a status label (shown as spinner text in TUI). No-op by default.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send_status(
        &mut self,
        _text: &str,
    ) -> impl Future<Output = Result<(), ChannelError>> + Send {
        async { Ok(()) }
    }

    /// Send a thinking/reasoning token chunk. No-op by default.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send_thinking_chunk(
        &mut self,
        _chunk: &str,
    ) -> impl Future<Output = Result<(), ChannelError>> + Send {
        async { Ok(()) }
    }

    /// Notify channel of queued message count. No-op by default.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send_queue_count(
        &mut self,
        _count: usize,
    ) -> impl Future<Output = Result<(), ChannelError>> + Send {
        async { Ok(()) }
    }

    /// Send token usage after an LLM call. No-op by default.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send_usage(
        &mut self,
        _input_tokens: u64,
        _output_tokens: u64,
        _context_window: u64,
    ) -> impl Future<Output = Result<(), ChannelError>> + Send {
        async { Ok(()) }
    }

    /// Send diff data for a tool result. No-op by default (TUI overrides).
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send_diff(
        &mut self,
        _diff: crate::DiffData,
    ) -> impl Future<Output = Result<(), ChannelError>> + Send {
        async { Ok(()) }
    }

    /// Announce that a tool call is starting.
    ///
    /// Emitted before execution begins so the transport layer can send an
    /// `InProgress` status to the peer before the result arrives.
    /// No-op by default.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send_tool_start(
        &mut self,
        _tool_name: &str,
        _tool_call_id: &str,
        _params: Option<serde_json::Value>,
        _parent_tool_use_id: Option<String>,
    ) -> impl Future<Output = Result<(), ChannelError>> + Send {
        async { Ok(()) }
    }

    /// Send a complete tool output with optional diff and filter stats atomically.
    ///
    /// `body` is the raw tool output content (no header). The default implementation
    /// formats it with `[tool output: <name>]` prefix for human-readable channels.
    /// Structured channels (e.g. `LoopbackChannel`) override this to emit a typed event
    /// so consumers can access `tool_name` and `body` as separate fields.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    #[allow(clippy::too_many_arguments)]
    fn send_tool_output(
        &mut self,
        tool_name: &str,
        body: &str,
        _diff: Option<crate::DiffData>,
        _filter_stats: Option<String>,
        _kept_lines: Option<Vec<usize>>,
        _locations: Option<Vec<String>>,
        _tool_call_id: &str,
        _is_error: bool,
        _parent_tool_use_id: Option<String>,
        _raw_response: Option<serde_json::Value>,
        _started_at: Option<std::time::Instant>,
    ) -> impl Future<Output = Result<(), ChannelError>> + Send {
        let formatted = crate::agent::format_tool_output(tool_name, body);
        async move { self.send(&formatted).await }
    }

    /// Request user confirmation for a destructive action. Returns `true` if confirmed.
    /// Default: auto-confirm (for headless/test scenarios).
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn confirm(
        &mut self,
        _prompt: &str,
    ) -> impl Future<Output = Result<bool, ChannelError>> + Send {
        async { Ok(true) }
    }
}

/// Events emitted by the agent side toward the A2A caller.
#[derive(Debug, Clone)]
pub enum LoopbackEvent {
    Chunk(String),
    Flush,
    FullMessage(String),
    Status(String),
    /// Emitted immediately before tool execution begins.
    ToolStart {
        tool_name: String,
        tool_call_id: String,
        /// Raw input parameters passed to the tool (e.g. `{"command": "..."}` for bash).
        params: Option<serde_json::Value>,
        /// Set when this tool call is made by a subagent; identifies the parent's `tool_call_id`.
        parent_tool_use_id: Option<String>,
        /// Wall-clock instant when the tool call was initiated; used to compute elapsed time.
        started_at: std::time::Instant,
    },
    ToolOutput {
        tool_name: String,
        display: String,
        diff: Option<crate::DiffData>,
        filter_stats: Option<String>,
        kept_lines: Option<Vec<usize>>,
        locations: Option<Vec<String>>,
        tool_call_id: String,
        is_error: bool,
        /// Terminal ID for shell tool calls routed through the IDE terminal.
        terminal_id: Option<String>,
        /// Set when this tool output belongs to a subagent; identifies the parent's `tool_call_id`.
        parent_tool_use_id: Option<String>,
        /// Structured tool response payload for ACP intermediate `tool_call_update` notifications.
        raw_response: Option<serde_json::Value>,
        /// Wall-clock instant when the corresponding `ToolStart` was emitted; used for elapsed time.
        started_at: Option<std::time::Instant>,
    },
    /// Token usage from the last LLM turn.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        context_window: u64,
    },
    /// Generated session title (emitted after the first agent response).
    SessionTitle(String),
    /// Execution plan update.
    Plan(Vec<(String, PlanItemStatus)>),
    /// Thinking/reasoning token chunk from the LLM.
    ThinkingChunk(String),
}

/// Status of a plan item, mirroring `acp::PlanEntryStatus`.
#[derive(Debug, Clone)]
pub enum PlanItemStatus {
    Pending,
    InProgress,
    Completed,
}

/// Caller-side handle for sending input and receiving agent output.
pub struct LoopbackHandle {
    pub input_tx: tokio::sync::mpsc::Sender<ChannelMessage>,
    pub output_rx: tokio::sync::mpsc::Receiver<LoopbackEvent>,
    /// Shared cancel signal: notify to interrupt the agent's current operation.
    pub cancel_signal: std::sync::Arc<tokio::sync::Notify>,
}

/// Headless channel bridging an A2A `TaskProcessor` with the agent loop.
pub struct LoopbackChannel {
    input_rx: tokio::sync::mpsc::Receiver<ChannelMessage>,
    output_tx: tokio::sync::mpsc::Sender<LoopbackEvent>,
}

impl LoopbackChannel {
    /// Create a linked `(LoopbackChannel, LoopbackHandle)` pair.
    #[must_use]
    pub fn pair(buffer: usize) -> (Self, LoopbackHandle) {
        let (input_tx, input_rx) = tokio::sync::mpsc::channel(buffer);
        let (output_tx, output_rx) = tokio::sync::mpsc::channel(buffer);
        let cancel_signal = std::sync::Arc::new(tokio::sync::Notify::new());
        (
            Self {
                input_rx,
                output_tx,
            },
            LoopbackHandle {
                input_tx,
                output_rx,
                cancel_signal,
            },
        )
    }
}

impl Channel for LoopbackChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        Ok(self.input_rx.recv().await)
    }

    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        self.output_tx
            .send(LoopbackEvent::FullMessage(text.to_owned()))
            .await
            .map_err(|_| ChannelError::ChannelClosed)
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        self.output_tx
            .send(LoopbackEvent::Chunk(chunk.to_owned()))
            .await
            .map_err(|_| ChannelError::ChannelClosed)
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        self.output_tx
            .send(LoopbackEvent::Flush)
            .await
            .map_err(|_| ChannelError::ChannelClosed)
    }

    async fn send_status(&mut self, text: &str) -> Result<(), ChannelError> {
        self.output_tx
            .send(LoopbackEvent::Status(text.to_owned()))
            .await
            .map_err(|_| ChannelError::ChannelClosed)
    }

    async fn send_thinking_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        self.output_tx
            .send(LoopbackEvent::ThinkingChunk(chunk.to_owned()))
            .await
            .map_err(|_| ChannelError::ChannelClosed)
    }

    async fn send_tool_start(
        &mut self,
        tool_name: &str,
        tool_call_id: &str,
        params: Option<serde_json::Value>,
        parent_tool_use_id: Option<String>,
    ) -> Result<(), ChannelError> {
        self.output_tx
            .send(LoopbackEvent::ToolStart {
                tool_name: tool_name.to_owned(),
                tool_call_id: tool_call_id.to_owned(),
                params,
                parent_tool_use_id,
                started_at: std::time::Instant::now(),
            })
            .await
            .map_err(|_| ChannelError::ChannelClosed)
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_tool_output(
        &mut self,
        tool_name: &str,
        body: &str,
        diff: Option<crate::DiffData>,
        filter_stats: Option<String>,
        kept_lines: Option<Vec<usize>>,
        locations: Option<Vec<String>>,
        tool_call_id: &str,
        is_error: bool,
        parent_tool_use_id: Option<String>,
        raw_response: Option<serde_json::Value>,
        started_at: Option<std::time::Instant>,
    ) -> Result<(), ChannelError> {
        self.output_tx
            .send(LoopbackEvent::ToolOutput {
                tool_name: tool_name.to_owned(),
                display: body.to_owned(),
                diff,
                filter_stats,
                kept_lines,
                locations,
                tool_call_id: tool_call_id.to_owned(),
                is_error,
                terminal_id: None,
                parent_tool_use_id,
                raw_response,
                started_at,
            })
            .await
            .map_err(|_| ChannelError::ChannelClosed)
    }

    async fn confirm(&mut self, _prompt: &str) -> Result<bool, ChannelError> {
        Ok(true)
    }

    async fn send_usage(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        context_window: u64,
    ) -> Result<(), ChannelError> {
        self.output_tx
            .send(LoopbackEvent::Usage {
                input_tokens,
                output_tokens,
                context_window,
            })
            .await
            .map_err(|_| ChannelError::ChannelClosed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_message_creation() {
        let msg = ChannelMessage {
            text: "hello".to_string(),
            attachments: vec![],
        };
        assert_eq!(msg.text, "hello");
        assert!(msg.attachments.is_empty());
    }

    struct StubChannel;

    impl Channel for StubChannel {
        async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
            Ok(None)
        }

        async fn send(&mut self, _text: &str) -> Result<(), ChannelError> {
            Ok(())
        }

        async fn send_chunk(&mut self, _chunk: &str) -> Result<(), ChannelError> {
            Ok(())
        }

        async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn send_chunk_default_is_noop() {
        let mut ch = StubChannel;
        ch.send_chunk("partial").await.unwrap();
    }

    #[tokio::test]
    async fn flush_chunks_default_is_noop() {
        let mut ch = StubChannel;
        ch.flush_chunks().await.unwrap();
    }

    #[tokio::test]
    async fn stub_channel_confirm_auto_approves() {
        let mut ch = StubChannel;
        let result = ch.confirm("Delete everything?").await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn stub_channel_send_typing_default() {
        let mut ch = StubChannel;
        ch.send_typing().await.unwrap();
    }

    #[tokio::test]
    async fn stub_channel_recv_returns_none() {
        let mut ch = StubChannel;
        let msg = ch.recv().await.unwrap();
        assert!(msg.is_none());
    }

    #[tokio::test]
    async fn stub_channel_send_ok() {
        let mut ch = StubChannel;
        ch.send("hello").await.unwrap();
    }

    #[test]
    fn channel_message_clone() {
        let msg = ChannelMessage {
            text: "test".to_string(),
            attachments: vec![],
        };
        let cloned = msg.clone();
        assert_eq!(cloned.text, "test");
    }

    #[test]
    fn channel_message_debug() {
        let msg = ChannelMessage {
            text: "debug".to_string(),
            attachments: vec![],
        };
        let debug = format!("{msg:?}");
        assert!(debug.contains("debug"));
    }

    #[test]
    fn attachment_kind_equality() {
        assert_eq!(AttachmentKind::Audio, AttachmentKind::Audio);
        assert_ne!(AttachmentKind::Audio, AttachmentKind::Image);
    }

    #[test]
    fn attachment_construction() {
        let a = Attachment {
            kind: AttachmentKind::Audio,
            data: vec![0, 1, 2],
            filename: Some("test.wav".into()),
        };
        assert_eq!(a.kind, AttachmentKind::Audio);
        assert_eq!(a.data.len(), 3);
        assert_eq!(a.filename.as_deref(), Some("test.wav"));
    }

    #[test]
    fn channel_message_with_attachments() {
        let msg = ChannelMessage {
            text: String::new(),
            attachments: vec![Attachment {
                kind: AttachmentKind::Audio,
                data: vec![42],
                filename: None,
            }],
        };
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].kind, AttachmentKind::Audio);
    }

    #[test]
    fn stub_channel_try_recv_returns_none() {
        let mut ch = StubChannel;
        assert!(ch.try_recv().is_none());
    }

    #[tokio::test]
    async fn stub_channel_send_queue_count_noop() {
        let mut ch = StubChannel;
        ch.send_queue_count(5).await.unwrap();
    }

    // LoopbackChannel tests

    #[test]
    fn loopback_pair_returns_linked_handles() {
        let (channel, handle) = LoopbackChannel::pair(8);
        // Both sides exist and channels are connected via their sender capacity
        drop(channel);
        drop(handle);
    }

    #[tokio::test]
    async fn loopback_cancel_signal_can_be_notified_and_awaited() {
        let (_channel, handle) = LoopbackChannel::pair(8);
        let signal = std::sync::Arc::clone(&handle.cancel_signal);
        // Notify from one side, await on the other.
        let notified = signal.notified();
        handle.cancel_signal.notify_one();
        notified.await; // resolves immediately after notify_one()
    }

    #[tokio::test]
    async fn loopback_cancel_signal_shared_across_clones() {
        let (_channel, handle) = LoopbackChannel::pair(8);
        let signal_a = std::sync::Arc::clone(&handle.cancel_signal);
        let signal_b = std::sync::Arc::clone(&handle.cancel_signal);
        let notified = signal_b.notified();
        signal_a.notify_one();
        notified.await;
    }

    #[tokio::test]
    async fn loopback_send_recv_round_trip() {
        let (mut channel, handle) = LoopbackChannel::pair(8);
        handle
            .input_tx
            .send(ChannelMessage {
                text: "hello".to_owned(),
                attachments: vec![],
            })
            .await
            .unwrap();
        let msg = channel.recv().await.unwrap().unwrap();
        assert_eq!(msg.text, "hello");
    }

    #[tokio::test]
    async fn loopback_recv_returns_none_when_handle_dropped() {
        let (mut channel, handle) = LoopbackChannel::pair(8);
        drop(handle);
        let result = channel.recv().await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn loopback_send_produces_full_message_event() {
        let (mut channel, mut handle) = LoopbackChannel::pair(8);
        channel.send("world").await.unwrap();
        let event = handle.output_rx.recv().await.unwrap();
        assert!(matches!(event, LoopbackEvent::FullMessage(t) if t == "world"));
    }

    #[tokio::test]
    async fn loopback_send_chunk_then_flush() {
        let (mut channel, mut handle) = LoopbackChannel::pair(8);
        channel.send_chunk("part1").await.unwrap();
        channel.flush_chunks().await.unwrap();
        let ev1 = handle.output_rx.recv().await.unwrap();
        let ev2 = handle.output_rx.recv().await.unwrap();
        assert!(matches!(ev1, LoopbackEvent::Chunk(t) if t == "part1"));
        assert!(matches!(ev2, LoopbackEvent::Flush));
    }

    #[tokio::test]
    async fn loopback_send_tool_output() {
        let (mut channel, mut handle) = LoopbackChannel::pair(8);
        channel
            .send_tool_output(
                "bash", "exit 0", None, None, None, None, "", false, None, None, None,
            )
            .await
            .unwrap();
        let event = handle.output_rx.recv().await.unwrap();
        match event {
            LoopbackEvent::ToolOutput {
                tool_name,
                display,
                diff,
                filter_stats,
                kept_lines,
                locations,
                tool_call_id,
                is_error,
                terminal_id,
                parent_tool_use_id,
                raw_response,
                ..
            } => {
                assert_eq!(tool_name, "bash");
                assert_eq!(display, "exit 0");
                assert!(diff.is_none());
                assert!(filter_stats.is_none());
                assert!(kept_lines.is_none());
                assert!(locations.is_none());
                assert_eq!(tool_call_id, "");
                assert!(!is_error);
                assert!(terminal_id.is_none());
                assert!(parent_tool_use_id.is_none());
                assert!(raw_response.is_none());
            }
            _ => panic!("expected ToolOutput event"),
        }
    }

    #[tokio::test]
    async fn loopback_confirm_auto_approves() {
        let (mut channel, _handle) = LoopbackChannel::pair(8);
        let result = channel.confirm("are you sure?").await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn loopback_send_error_when_output_closed() {
        let (mut channel, handle) = LoopbackChannel::pair(8);
        // Drop only the output_rx side by dropping the handle
        drop(handle);
        let result = channel.send("too late").await;
        assert!(matches!(result, Err(ChannelError::ChannelClosed)));
    }

    #[tokio::test]
    async fn loopback_send_chunk_error_when_output_closed() {
        let (mut channel, handle) = LoopbackChannel::pair(8);
        drop(handle);
        let result = channel.send_chunk("chunk").await;
        assert!(matches!(result, Err(ChannelError::ChannelClosed)));
    }

    #[tokio::test]
    async fn loopback_flush_error_when_output_closed() {
        let (mut channel, handle) = LoopbackChannel::pair(8);
        drop(handle);
        let result = channel.flush_chunks().await;
        assert!(matches!(result, Err(ChannelError::ChannelClosed)));
    }

    #[tokio::test]
    async fn loopback_send_status_event() {
        let (mut channel, mut handle) = LoopbackChannel::pair(8);
        channel.send_status("working...").await.unwrap();
        let event = handle.output_rx.recv().await.unwrap();
        assert!(matches!(event, LoopbackEvent::Status(s) if s == "working..."));
    }

    #[tokio::test]
    async fn loopback_send_usage_produces_usage_event() {
        let (mut channel, mut handle) = LoopbackChannel::pair(8);
        channel.send_usage(100, 50, 200_000).await.unwrap();
        let event = handle.output_rx.recv().await.unwrap();
        match event {
            LoopbackEvent::Usage {
                input_tokens,
                output_tokens,
                context_window,
            } => {
                assert_eq!(input_tokens, 100);
                assert_eq!(output_tokens, 50);
                assert_eq!(context_window, 200_000);
            }
            _ => panic!("expected Usage event"),
        }
    }

    #[tokio::test]
    async fn loopback_send_usage_error_when_closed() {
        let (mut channel, handle) = LoopbackChannel::pair(8);
        drop(handle);
        let result = channel.send_usage(1, 2, 3).await;
        assert!(matches!(result, Err(ChannelError::ChannelClosed)));
    }

    #[test]
    fn plan_item_status_variants_are_distinct() {
        assert!(!matches!(
            PlanItemStatus::Pending,
            PlanItemStatus::InProgress
        ));
        assert!(!matches!(
            PlanItemStatus::InProgress,
            PlanItemStatus::Completed
        ));
        assert!(!matches!(
            PlanItemStatus::Completed,
            PlanItemStatus::Pending
        ));
    }

    #[test]
    fn loopback_event_session_title_carries_string() {
        let event = LoopbackEvent::SessionTitle("hello".to_owned());
        assert!(matches!(event, LoopbackEvent::SessionTitle(s) if s == "hello"));
    }

    #[test]
    fn loopback_event_plan_carries_entries() {
        let entries = vec![
            ("step 1".to_owned(), PlanItemStatus::Pending),
            ("step 2".to_owned(), PlanItemStatus::InProgress),
        ];
        let event = LoopbackEvent::Plan(entries);
        match event {
            LoopbackEvent::Plan(e) => {
                assert_eq!(e.len(), 2);
                assert!(matches!(e[0].1, PlanItemStatus::Pending));
                assert!(matches!(e[1].1, PlanItemStatus::InProgress));
            }
            _ => panic!("expected Plan event"),
        }
    }
}
