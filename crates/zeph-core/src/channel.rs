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

    /// Send a complete tool output with optional diff and filter stats atomically.
    ///
    /// The default implementation calls [`Self::send`] with the pre-formatted display text.
    /// TUI overrides this to emit a single event that creates the Tool message and attaches
    /// diff/filter data without a race between separate sends.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send_tool_output(
        &mut self,
        _tool_name: &str,
        display: &str,
        _diff: Option<crate::DiffData>,
        _filter_stats: Option<String>,
    ) -> impl Future<Output = Result<(), ChannelError>> + Send {
        self.send(display)
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
}
