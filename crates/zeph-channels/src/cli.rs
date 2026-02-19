use std::collections::VecDeque;

use zeph_core::channel::{Attachment, AttachmentKind, Channel, ChannelError, ChannelMessage};

use crate::line_editor::{self, ReadLineResult};

type PersistFn = Box<dyn Fn(&str) + Send>;

struct InputHistory {
    entries: VecDeque<String>,
    persist_fn: PersistFn,
    max_len: usize,
}

impl InputHistory {
    fn new(entries: Vec<String>, persist_fn: PersistFn) -> Self {
        Self {
            entries: VecDeque::from(entries),
            persist_fn,
            max_len: 1000,
        }
    }

    fn entries(&self) -> &VecDeque<String> {
        &self.entries
    }

    fn add(&mut self, line: &str) {
        if line.is_empty() {
            return;
        }
        if self.entries.back().is_some_and(|last| last == line) {
            return;
        }
        if self.entries.len() == self.max_len {
            self.entries.pop_front();
        }
        self.entries.push_back(line.to_owned());
        (self.persist_fn)(line);
    }
}

impl std::fmt::Debug for InputHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputHistory")
            .field("entries_len", &self.entries.len())
            .finish_non_exhaustive()
    }
}

/// CLI channel that reads from stdin and writes to stdout.
#[derive(Debug)]
pub struct CliChannel {
    accumulated: String,
    history: Option<InputHistory>,
}

impl CliChannel {
    #[must_use]
    pub fn new() -> Self {
        Self {
            accumulated: String::new(),
            history: None,
        }
    }

    /// Create a CLI channel with persistent history.
    ///
    /// `entries` should be pre-loaded by the caller. `persist_fn` is called
    /// for each new entry to persist it (e.g. via `SqliteStore::save_input_entry`).
    #[must_use]
    pub fn with_history(entries: Vec<String>, persist_fn: impl Fn(&str) + Send + 'static) -> Self {
        Self {
            accumulated: String::new(),
            history: Some(InputHistory::new(entries, Box::new(persist_fn))),
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
        let entries: Vec<String> = self
            .history
            .as_ref()
            .map(|h| h.entries().iter().cloned().collect())
            .unwrap_or_default();

        let result = tokio::task::spawn_blocking(move || line_editor::read_line("You: ", &entries))
            .await
            .map_err(|e| ChannelError::Other(e.to_string()))?
            .map_err(ChannelError::Io)?;

        let line = match result {
            ReadLineResult::Interrupted | ReadLineResult::Eof => return Ok(None),
            ReadLineResult::Line(l) => l,
        };

        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "exit" || trimmed == "quit" {
            return Ok(None);
        }

        if let Some(h) = &mut self.history {
            h.add(trimmed);
        }

        self.accumulated.clear();

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
        let prompt = format!("{prompt} [y/N]: ");
        let result = tokio::task::spawn_blocking(move || line_editor::read_line(&prompt, &[]))
            .await
            .map_err(|e| ChannelError::Other(e.to_string()))?
            .map_err(ChannelError::Io)?;

        match result {
            ReadLineResult::Line(line) => Ok(line.trim().eq_ignore_ascii_case("y")),
            ReadLineResult::Interrupted | ReadLineResult::Eof => Ok(false),
        }
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
        let err = ChannelError::Io(result.unwrap_err());
        assert!(matches!(err, ChannelError::Io(_)));
    }

    #[test]
    fn image_command_empty_args_detected() {
        let trimmed = "/image";
        let arg = trimmed.strip_prefix("/image").map(str::trim).unwrap_or("");
        assert!(arg.is_empty());

        let trimmed_space = "/image   ";
        let arg_space = trimmed_space
            .strip_prefix("/image")
            .map(str::trim)
            .unwrap_or("");
        assert!(arg_space.is_empty());
    }

    #[test]
    fn input_history_add_and_dedup() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let persisted = Arc::new(AtomicUsize::new(0));
        let p = persisted.clone();
        let mut history = InputHistory::new(
            vec![],
            Box::new(move |_| {
                p.fetch_add(1, Ordering::Relaxed);
            }),
        );
        history.add("hello");
        history.add("hello"); // duplicate
        history.add("world");
        assert_eq!(history.entries().len(), 2);
        assert_eq!(history.entries()[0], "hello");
        assert_eq!(persisted.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn input_history_ignores_empty() {
        let mut history = InputHistory::new(vec![], Box::new(|_| {}));
        history.add("");
        assert_eq!(history.entries().len(), 0);
    }
}
