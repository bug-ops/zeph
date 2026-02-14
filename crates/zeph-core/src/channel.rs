/// Incoming message from a channel.
#[derive(Debug, Clone)]
pub struct ChannelMessage {
    pub text: String,
}

/// Bidirectional communication channel for the agent.
pub trait Channel: Send {
    /// Receive the next message. Returns `None` on EOF or shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn recv(&mut self) -> impl Future<Output = anyhow::Result<Option<ChannelMessage>>> + Send;

    /// Non-blocking receive. Returns `None` if no message is immediately available.
    fn try_recv(&mut self) -> Option<ChannelMessage> {
        None
    }

    /// Send a text response.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send(&mut self, text: &str) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Send a partial chunk of streaming response.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send_chunk(&mut self, chunk: &str) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Flush any buffered chunks.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn flush_chunks(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Send a typing indicator. No-op by default.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send_typing(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send {
        async { Ok(()) }
    }

    /// Send a status label (shown as spinner text in TUI). No-op by default.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn send_status(&mut self, _text: &str) -> impl Future<Output = anyhow::Result<()>> + Send {
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
    ) -> impl Future<Output = anyhow::Result<()>> + Send {
        async { Ok(()) }
    }

    /// Request user confirmation for a destructive action. Returns `true` if confirmed.
    /// Default: auto-confirm (for headless/test scenarios).
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying I/O fails.
    fn confirm(&mut self, _prompt: &str) -> impl Future<Output = anyhow::Result<bool>> + Send {
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
        };
        assert_eq!(msg.text, "hello");
    }

    struct StubChannel;

    impl Channel for StubChannel {
        async fn recv(&mut self) -> anyhow::Result<Option<ChannelMessage>> {
            Ok(None)
        }

        async fn send(&mut self, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn send_chunk(&mut self, _chunk: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn flush_chunks(&mut self) -> anyhow::Result<()> {
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
        };
        let cloned = msg.clone();
        assert_eq!(cloned.text, "test");
    }

    #[test]
    fn channel_message_debug() {
        let msg = ChannelMessage {
            text: "debug".to_string(),
        };
        let debug = format!("{msg:?}");
        assert!(debug.contains("debug"));
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
