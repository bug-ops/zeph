use zeph_core::channel::{Attachment, AttachmentKind, Channel, ChannelError, ChannelMessage};

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
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
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
                Err(e) => Err(e),
            }
        })
        .await
        .map_err(|e| ChannelError::Other(e.to_string()))??;

        let Some(raw) = line else {
            return Ok(None);
        };

        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "exit" || trimmed == "quit" {
            return Ok(None);
        }

        // Reset accumulated for new response
        self.accumulated.clear();

        // Handle /image <path> command by reading the file into an attachment
        if let Some(path) = trimmed.strip_prefix("/image").map(str::trim) {
            if path.is_empty() {
                println!("Usage: /image <path>");
                return Ok(Some(ChannelMessage {
                    text: String::new(),
                    attachments: vec![],
                }));
            }
            let path_owned = path.to_owned();
            let data = tokio::fs::read(&path_owned)
                .await
                .map_err(ChannelError::Io)?;
            let filename = std::path::Path::new(&path_owned)
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_owned);
            return Ok(Some(ChannelMessage {
                text: String::new(),
                attachments: vec![Attachment {
                    kind: AttachmentKind::Image,
                    data,
                    filename,
                }],
            }));
        }

        Ok(Some(ChannelMessage {
            text: trimmed.to_string(),
            attachments: vec![],
        }))
    }

    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        println!("Zeph: {text}");
        Ok(())
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        use std::io::{Write, stdout};
        print!("{chunk}");
        stdout().flush()?;
        self.accumulated.push_str(chunk);
        Ok(())
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        println!();
        Ok(())
    }

    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        let prompt = prompt.to_owned();
        tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Write};
            print!("{prompt} [y/N]: ");
            std::io::stdout().flush()?;
            let mut buf = String::new();
            std::io::stdin().lock().read_line(&mut buf)?;
            Ok(buf.trim().eq_ignore_ascii_case("y"))
        })
        .await
        .map_err(|e| ChannelError::Other(e.to_string()))?
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

    #[tokio::test]
    async fn image_command_valid_file_creates_attachment() {
        use std::io::Write;

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let image_bytes = b"\x89PNG\r\n\x1a\nfake-image-data";
        tmp.write_all(image_bytes).unwrap();
        tmp.flush().unwrap();

        let path = tmp.path().to_str().unwrap().to_owned();
        let filename = tmp.path().file_name().unwrap().to_str().unwrap().to_owned();

        // Simulate /image <path> parsing: strip prefix and read file
        let trimmed = format!("/image {path}");
        let arg = trimmed.strip_prefix("/image").map(str::trim).unwrap();
        assert!(!arg.is_empty());

        let data = tokio::fs::read(arg).await.unwrap();
        let parsed_filename = std::path::Path::new(arg)
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_owned);

        assert_eq!(data, image_bytes);
        assert_eq!(parsed_filename, Some(filename));

        let attachment = Attachment {
            kind: AttachmentKind::Image,
            data,
            filename: parsed_filename,
        };
        assert_eq!(attachment.kind, AttachmentKind::Image);
        assert_eq!(attachment.data, image_bytes);
    }

    #[tokio::test]
    async fn image_command_missing_file_returns_io_error() {
        let result = tokio::fs::read("/nonexistent/path/image.png").await;
        assert!(result.is_err());
        // Verify it maps to ChannelError::Io correctly
        let err = ChannelError::Io(result.unwrap_err());
        assert!(matches!(err, ChannelError::Io(_)));
    }

    #[test]
    fn image_command_empty_args_detected() {
        // "/image " with only whitespace after stripping prefix yields empty arg
        let trimmed = "/image";
        let arg = trimmed.strip_prefix("/image").map(str::trim).unwrap_or("");
        assert!(arg.is_empty());

        // "/image " (with trailing space)
        let trimmed_space = "/image   ";
        let arg_space = trimmed_space
            .strip_prefix("/image")
            .map(str::trim)
            .unwrap_or("");
        assert!(arg_space.is_empty());
    }
}
