use zeph_core::channel::{Channel, ChannelMessage};

/// CLI channel that reads from stdin and writes to stdout.
#[derive(Debug)]
pub struct CliChannel {
    accumulated: String,
}

impl CliChannel {
    #[must_use]
    pub fn new() -> Self {
        Self {
            accumulated: String::new(),
        }
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

        // Reset accumulated for new response
        self.accumulated.clear();

        Ok(Some(ChannelMessage {
            text: trimmed.to_string(),
        }))
    }

    async fn send(&mut self, text: &str) -> anyhow::Result<()> {
        println!("Zeph: {text}");
        Ok(())
    }

    async fn send_chunk(&mut self, chunk: &str) -> anyhow::Result<()> {
        use std::io::{Write, stdout};
        print!("{chunk}");
        stdout().flush()?;
        self.accumulated.push_str(chunk);
        Ok(())
    }

    async fn flush_chunks(&mut self) -> anyhow::Result<()> {
        println!();
        Ok(())
    }

    async fn confirm(&mut self, prompt: &str) -> anyhow::Result<bool> {
        let prompt = prompt.to_owned();
        tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Write};
            print!("{prompt} [y/N]: ");
            std::io::stdout().flush()?;
            let mut buf = String::new();
            std::io::stdin().lock().read_line(&mut buf)?;
            Ok(buf.trim().eq_ignore_ascii_case("y"))
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_channel_default() {
        let ch = CliChannel::default();
        let _ = format!("{ch:?}");
    }

    #[tokio::test]
    async fn cli_channel_send_chunk_accumulates() {
        let mut ch = CliChannel::new();
        ch.send_chunk("hello").await.unwrap();
        ch.send_chunk(" ").await.unwrap();
        ch.send_chunk("world").await.unwrap();
        assert_eq!(ch.accumulated, "hello world");
    }

    #[tokio::test]
    async fn cli_channel_flush_chunks_retains_buffer() {
        let mut ch = CliChannel::new();
        ch.send_chunk("test").await.unwrap();
        ch.flush_chunks().await.unwrap();
        assert_eq!(ch.accumulated, "test");
    }

    #[test]
    fn cli_channel_try_recv_returns_none() {
        let mut ch = CliChannel::new();
        assert!(ch.try_recv().is_none());
    }

    #[test]
    fn cli_channel_new() {
        let ch = CliChannel::new();
        assert!(ch.accumulated.is_empty());
    }

    #[tokio::test]
    async fn cli_channel_send_returns_ok() {
        let mut ch = CliChannel::new();
        ch.send("test message").await.unwrap();
    }

    #[tokio::test]
    async fn cli_channel_flush_returns_ok() {
        let mut ch = CliChannel::new();
        ch.flush_chunks().await.unwrap();
    }
}
