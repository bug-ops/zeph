// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// A single field in an elicitation form request.
///
/// Created by the MCP layer when a server sends an elicitation request; passed to
/// channels so they can render the field in a channel-appropriate way (CLI prompt,
/// Telegram inline keyboard, TUI form, etc.).
///
/// # Examples
///
/// ```
/// use zeph_core::channel::{ElicitationField, ElicitationFieldType};
///
/// let field = ElicitationField {
///     name: "username".to_owned(),
///     description: Some("Your login name".to_owned()),
///     field_type: ElicitationFieldType::String,
///     required: true,
/// };
/// assert_eq!(field.name, "username");
/// assert!(field.required);
/// ```
#[derive(Debug, Clone)]
pub struct ElicitationField {
    /// Field key as declared in the server's JSON Schema (sanitized before display).
    pub name: String,
    /// Optional human-readable description from the server (sanitized before display).
    pub description: Option<String>,
    /// Value type expected for this field.
    pub field_type: ElicitationFieldType,
    /// Whether the field must be filled before the form can be submitted.
    pub required: bool,
}

/// Type of an elicitation form field.
///
/// # Examples
///
/// ```
/// use zeph_core::channel::ElicitationFieldType;
///
/// let enum_field = ElicitationFieldType::Enum(vec!["low".into(), "medium".into(), "high".into()]);
/// assert!(matches!(enum_field, ElicitationFieldType::Enum(_)));
/// ```
#[derive(Debug, Clone)]
pub enum ElicitationFieldType {
    String,
    Integer,
    Number,
    Boolean,
    /// Enum with allowed values (sanitized before display).
    Enum(Vec<String>),
}

/// An elicitation request from an MCP server.
///
/// Channels receive this struct and are responsible for rendering the form and
/// collecting user input. The `server_name` must be shown to help users identify
/// which server is requesting information (phishing prevention).
///
/// # Examples
///
/// ```
/// use zeph_core::channel::{ElicitationField, ElicitationFieldType, ElicitationRequest};
///
/// let req = ElicitationRequest {
///     server_name: "my-server".to_owned(),
///     message: "Please provide your credentials".to_owned(),
///     fields: vec![ElicitationField {
///         name: "api_key".to_owned(),
///         description: None,
///         field_type: ElicitationFieldType::String,
///         required: true,
///     }],
/// };
/// assert_eq!(req.server_name, "my-server");
/// assert_eq!(req.fields.len(), 1);
/// ```
#[derive(Debug, Clone)]
pub struct ElicitationRequest {
    /// Name of the MCP server making the request (shown for phishing prevention).
    pub server_name: String,
    /// Human-readable message from the server.
    pub message: String,
    /// Form fields to collect from the user.
    pub fields: Vec<ElicitationField>,
}

/// User's response to an elicitation request.
///
/// Channels return this after the user interacts with the form. The MCP layer
/// maps `Declined` and `Cancelled` to the appropriate protocol responses.
///
/// # Examples
///
/// ```
/// use serde_json::json;
/// use zeph_core::channel::ElicitationResponse;
///
/// let accepted = ElicitationResponse::Accepted(json!({"username": "alice"}));
/// assert!(matches!(accepted, ElicitationResponse::Accepted(_)));
///
/// let declined = ElicitationResponse::Declined;
/// assert!(matches!(declined, ElicitationResponse::Declined));
/// ```
#[derive(Debug, Clone)]
pub enum ElicitationResponse {
    /// User filled in the form and submitted.
    Accepted(serde_json::Value),
    /// User actively declined to provide input.
    Declined,
    /// User cancelled (e.g. Escape, timeout).
    Cancelled,
}

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

    /// No active session is established yet (no message has been received).
    ///
    /// Occurs when `send` or related methods are called before any message has
    /// arrived on the channel (i.e., `recv` has never returned successfully).
    #[error("no active session")]
    NoActiveSession,

    /// Catch-all for third-party API errors (Telegram, Discord, Slack, etc.)
    /// that do not map to a more specific variant.
    #[error("{0}")]
    Other(String),
}

impl ChannelError {
    /// Create a catch-all error from any displayable error.
    ///
    /// Converts the error message to a string and wraps it in the `Other` variant.
    /// Useful for wrapping provider-specific errors from third-party libraries.
    pub fn other(e: impl std::fmt::Display) -> Self {
        Self::Other(e.to_string())
    }
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
///
/// # TODO (A3 — deferred: split monolithic Channel into focused sub-traits)
///
/// `Channel` currently has 16+ methods with 12 default no-op bodies. This makes it easy to
/// accidentally ignore capabilities (e.g., streaming, elicitation) on a new channel
/// implementation without a compile error. The planned split:
///
/// - `MessageChannel` — `send` / `recv` (required for all channels)
/// - `StreamingChannel` — `send_streaming_chunk` / `finish_stream` (opt-in)
/// - `ElicitationChannel` — `request_elicitation` (opt-in)
/// - `StatusChannel` — `set_status` / `clear_status` (opt-in)
///
/// **Blocked by:** workspace-wide breaking change affecting CLI, Telegram, TUI, gateway, JSON,
/// Discord, Slack, loopback channels, and all integration tests. Must be migrated channel by
/// channel across ≥5 PRs. Requires its own SDD spec. See critic review §S4.
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

    /// Whether `/exit` and `/quit` commands should terminate the agent loop.
    ///
    /// Returns `false` for persistent server-side channels (e.g. Telegram) where
    /// breaking the loop would not meaningfully exit from the user's perspective.
    fn supports_exit(&self) -> bool {
        true
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
        _event: ToolStartEvent,
    ) -> impl Future<Output = Result<(), ChannelError>> + Send {
        async { Ok(()) }
    }

    /// Send a complete tool output with optional diff and filter stats atomically.
    ///
    /// `display` is the formatted tool output. The default implementation forwards to
    /// [`Channel::send`]. Structured channels (e.g. `LoopbackChannel`) override this to
    /// emit a typed event so consumers can access `tool_name` and `display` as separate fields.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send_tool_output(
        &mut self,
        event: ToolOutputEvent,
    ) -> impl Future<Output = Result<(), ChannelError>> + Send {
        let formatted = crate::agent::format_tool_output(event.tool_name.as_str(), &event.display);
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

    /// Request structured input from the user for an MCP elicitation.
    ///
    /// Always displays `request.server_name` to prevent phishing by malicious servers.
    /// Default: auto-decline (for headless/daemon/non-interactive scenarios).
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn elicit(
        &mut self,
        _request: ElicitationRequest,
    ) -> impl Future<Output = Result<ElicitationResponse, ChannelError>> + Send {
        async { Ok(ElicitationResponse::Declined) }
    }

    /// Signal the non-default stop reason to the consumer before flushing.
    ///
    /// Called by the agent loop immediately before `flush_chunks()` when a
    /// truncation or turn-limit condition is detected. No-op by default.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send_stop_hint(
        &mut self,
        _hint: StopHint,
    ) -> impl Future<Output = Result<(), ChannelError>> + Send {
        async { Ok(()) }
    }
}

/// Reason why the agent turn ended — carried by [`LoopbackEvent::Stop`].
///
/// Emitted by the agent loop immediately before `Flush` when a non-default
/// terminal condition is detected. Consumers (e.g. the ACP layer) map this to
/// the protocol-level `StopReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopHint {
    /// The LLM response was cut off by the token limit.
    MaxTokens,
    /// The turn loop exhausted `max_turns` without a final text response.
    MaxTurnRequests,
}

/// Event carrying data for a tool call start, emitted before execution begins.
///
/// Passed by value to [`Channel::send_tool_start`] and carried by
/// [`LoopbackEvent::ToolStart`]. All fields are owned — no lifetime parameters.
#[derive(Debug, Clone)]
pub struct ToolStartEvent {
    /// Name of the tool being invoked.
    pub tool_name: zeph_common::ToolName,
    /// Opaque tool call ID assigned by the LLM.
    pub tool_call_id: String,
    /// Raw input parameters passed to the tool (e.g. `{"command": "..."}` for bash).
    pub params: Option<serde_json::Value>,
    /// Set when this tool call is made by a subagent; identifies the parent's `tool_call_id`.
    pub parent_tool_use_id: Option<String>,
    /// Wall-clock instant when the tool call was initiated; used to compute elapsed time.
    pub started_at: std::time::Instant,
    /// True when this tool call was speculatively dispatched before LLM finished decoding.
    ///
    /// TUI renders a `[spec]` prefix; Telegram suppresses unless `chat_visibility = verbose`.
    pub speculative: bool,
    /// OS sandbox profile applied to this tool call, if any.
    ///
    /// `None` means no sandbox was applied (not configured or not a subprocess executor).
    pub sandbox_profile: Option<zeph_tools::SandboxProfile>,
}

/// Event carrying data for a completed tool output, emitted after execution.
///
/// Passed by value to [`Channel::send_tool_output`] and carried by
/// [`LoopbackEvent::ToolOutput`]. All fields are owned — no lifetime parameters.
#[derive(Debug, Clone)]
pub struct ToolOutputEvent {
    /// Name of the tool that produced this output.
    pub tool_name: zeph_common::ToolName,
    /// Human-readable output text.
    pub display: String,
    /// Optional diff for file-editing tools.
    pub diff: Option<crate::DiffData>,
    /// Optional filter statistics from output filtering.
    pub filter_stats: Option<String>,
    /// Kept line indices after filtering (for display).
    pub kept_lines: Option<Vec<usize>>,
    /// Source locations for code search results.
    pub locations: Option<Vec<String>>,
    /// Opaque tool call ID matching the corresponding `ToolStartEvent`.
    pub tool_call_id: String,
    /// Whether this output represents an error.
    pub is_error: bool,
    /// Terminal ID for shell tool calls routed through the IDE terminal.
    pub terminal_id: Option<String>,
    /// Set when this tool output belongs to a subagent; identifies the parent's `tool_call_id`.
    pub parent_tool_use_id: Option<String>,
    /// Structured tool response payload for ACP intermediate `tool_call_update` notifications.
    pub raw_response: Option<serde_json::Value>,
    /// Wall-clock instant when the corresponding `ToolStartEvent` was emitted.
    pub started_at: Option<std::time::Instant>,
}

/// Backward-compatible alias for [`ToolStartEvent`].
///
/// Kept for use in the ACP layer. Prefer [`ToolStartEvent`] in new code.
pub type ToolStartData = ToolStartEvent;

/// Backward-compatible alias for [`ToolOutputEvent`].
///
/// Kept for use in the ACP layer. Prefer [`ToolOutputEvent`] in new code.
pub type ToolOutputData = ToolOutputEvent;

/// Events emitted by the agent side toward the A2A caller.
#[derive(Debug, Clone)]
pub enum LoopbackEvent {
    Chunk(String),
    Flush,
    FullMessage(String),
    Status(String),
    /// Emitted immediately before tool execution begins.
    ToolStart(Box<ToolStartEvent>),
    ToolOutput(Box<ToolOutputEvent>),
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
    /// Non-default stop condition detected by the agent loop.
    ///
    /// Emitted immediately before `Flush`. When absent, the stop reason is `EndTurn`.
    Stop(StopHint),
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
    fn supports_exit(&self) -> bool {
        false
    }

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

    async fn send_tool_start(&mut self, event: ToolStartEvent) -> Result<(), ChannelError> {
        self.output_tx
            .send(LoopbackEvent::ToolStart(Box::new(event)))
            .await
            .map_err(|_| ChannelError::ChannelClosed)
    }

    async fn send_tool_output(&mut self, event: ToolOutputEvent) -> Result<(), ChannelError> {
        self.output_tx
            .send(LoopbackEvent::ToolOutput(Box::new(event)))
            .await
            .map_err(|_| ChannelError::ChannelClosed)
    }

    async fn confirm(&mut self, _prompt: &str) -> Result<bool, ChannelError> {
        Ok(true)
    }

    async fn send_stop_hint(&mut self, hint: StopHint) -> Result<(), ChannelError> {
        self.output_tx
            .send(LoopbackEvent::Stop(hint))
            .await
            .map_err(|_| ChannelError::ChannelClosed)
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

/// Adapter that wraps a [`Channel`] reference and implements [`zeph_commands::ChannelSink`].
///
/// Used at command dispatch time to coerce `&mut C` into `&mut dyn ChannelSink` without
/// a blanket impl (which would violate Rust's orphan rules).
pub(crate) struct ChannelSinkAdapter<'a, C: Channel>(pub &'a mut C);

impl<C: Channel> zeph_commands::ChannelSink for ChannelSinkAdapter<'_, C> {
    fn send<'a>(
        &'a mut self,
        msg: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), zeph_commands::CommandError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.0
                .send(msg)
                .await
                .map_err(zeph_commands::CommandError::new)
        })
    }

    fn flush_chunks<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), zeph_commands::CommandError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.0
                .flush_chunks()
                .await
                .map_err(zeph_commands::CommandError::new)
        })
    }

    fn send_queue_count<'a>(
        &'a mut self,
        count: usize,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), zeph_commands::CommandError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.0
                .send_queue_count(count)
                .await
                .map_err(zeph_commands::CommandError::new)
        })
    }

    fn supports_exit(&self) -> bool {
        self.0.supports_exit()
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
            .send_tool_output(ToolOutputEvent {
                tool_name: "bash".into(),
                display: "exit 0".into(),
                diff: None,
                filter_stats: None,
                kept_lines: None,
                locations: None,
                tool_call_id: String::new(),
                terminal_id: None,
                is_error: false,
                parent_tool_use_id: None,
                raw_response: None,
                started_at: None,
            })
            .await
            .unwrap();
        let event = handle.output_rx.recv().await.unwrap();
        match event {
            LoopbackEvent::ToolOutput(data) => {
                assert_eq!(data.tool_name, "bash");
                assert_eq!(data.display, "exit 0");
                assert!(data.diff.is_none());
                assert!(data.filter_stats.is_none());
                assert!(data.kept_lines.is_none());
                assert!(data.locations.is_none());
                assert_eq!(data.tool_call_id, "");
                assert!(!data.is_error);
                assert!(data.terminal_id.is_none());
                assert!(data.parent_tool_use_id.is_none());
                assert!(data.raw_response.is_none());
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

    #[tokio::test]
    async fn loopback_send_tool_start_produces_tool_start_event() {
        let (mut channel, mut handle) = LoopbackChannel::pair(8);
        channel
            .send_tool_start(ToolStartEvent {
                tool_name: "shell".into(),
                tool_call_id: "tc-001".into(),
                params: Some(serde_json::json!({"command": "ls"})),
                parent_tool_use_id: None,
                started_at: std::time::Instant::now(),
                speculative: false,
                sandbox_profile: None,
            })
            .await
            .unwrap();
        let event = handle.output_rx.recv().await.unwrap();
        match event {
            LoopbackEvent::ToolStart(data) => {
                assert_eq!(data.tool_name.as_str(), "shell");
                assert_eq!(data.tool_call_id.as_str(), "tc-001");
                assert!(data.params.is_some());
                assert!(data.parent_tool_use_id.is_none());
            }
            _ => panic!("expected ToolStart event"),
        }
    }

    #[tokio::test]
    async fn loopback_send_tool_start_with_parent_id() {
        let (mut channel, mut handle) = LoopbackChannel::pair(8);
        channel
            .send_tool_start(ToolStartEvent {
                tool_name: "web".into(),
                tool_call_id: "tc-002".into(),
                params: None,
                parent_tool_use_id: Some("parent-123".into()),
                started_at: std::time::Instant::now(),
                speculative: false,
                sandbox_profile: None,
            })
            .await
            .unwrap();
        let event = handle.output_rx.recv().await.unwrap();
        assert!(matches!(
            event,
            LoopbackEvent::ToolStart(ref data) if data.parent_tool_use_id.as_deref() == Some("parent-123")
        ));
    }

    #[tokio::test]
    async fn loopback_send_tool_start_error_when_output_closed() {
        let (mut channel, handle) = LoopbackChannel::pair(8);
        drop(handle);
        let result = channel
            .send_tool_start(ToolStartEvent {
                tool_name: "shell".into(),
                tool_call_id: "tc-003".into(),
                params: None,
                parent_tool_use_id: None,
                started_at: std::time::Instant::now(),
                speculative: false,
                sandbox_profile: None,
            })
            .await;
        assert!(matches!(result, Err(ChannelError::ChannelClosed)));
    }

    #[tokio::test]
    async fn default_send_tool_output_formats_message() {
        let mut ch = StubChannel;
        // Default impl calls self.send() which is a no-op in StubChannel — just verify it doesn't panic.
        ch.send_tool_output(ToolOutputEvent {
            tool_name: "bash".into(),
            display: "hello".into(),
            diff: None,
            filter_stats: None,
            kept_lines: None,
            locations: None,
            tool_call_id: "id".into(),
            terminal_id: None,
            is_error: false,
            parent_tool_use_id: None,
            raw_response: None,
            started_at: None,
        })
        .await
        .unwrap();
    }
}
