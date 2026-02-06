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
}

/// CLI channel that reads from stdin and writes to stdout.
#[derive(Debug)]
pub struct CliChannel {
    _private: (),
}

impl CliChannel {
    #[must_use]
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for CliChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl Channel for CliChannel {
    async fn recv(&mut self) -> anyhow::Result<Option<ChannelMessage>> {
        use std::io::{BufRead, Write};

        let line = tokio::task::spawn_blocking(|| {
            let stdin = std::io::stdin();
            let mut reader = stdin.lock();
            let mut buf = String::new();

            print!("You: ");
            std::io::stdout().flush()?;

            match reader.read_line(&mut buf) {
                Ok(0) => Ok(None),
                Ok(_) => Ok(Some(buf)),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        })
        .await??;

        let Some(raw) = line else {
            return Ok(None);
        };

        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "exit" || trimmed == "quit" {
            return Ok(None);
        }

        Ok(Some(ChannelMessage {
            text: trimmed.to_string(),
        }))
    }

    async fn send(&mut self, text: &str) -> anyhow::Result<()> {
        println!("Zeph: {text}");
        Ok(())
    }

    async fn send_chunk(&mut self, _chunk: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn flush_chunks(&mut self) -> anyhow::Result<()> {
        Ok(())
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

    #[test]
    fn cli_channel_default() {
        let ch = CliChannel::default();
        let _ = format!("{ch:?}");
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
}
