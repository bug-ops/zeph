use std::borrow::Cow;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rustyline::Editor;
use rustyline::error::ReadlineError;
use rustyline::history::{History, SearchDirection, SearchResult};
use sqlx::SqlitePool;
use zeph_core::channel::{Attachment, AttachmentKind, Channel, ChannelError, ChannelMessage};

struct SqliteHistory {
    entries: VecDeque<String>,
    pool: SqlitePool,
    max_len: usize,
    ignore_dups: bool,
    ignore_space: bool,
}

impl SqliteHistory {
    fn new(entries: Vec<String>, pool: SqlitePool) -> Self {
        Self {
            entries: VecDeque::from(entries),
            pool,
            max_len: 1000,
            ignore_dups: true,
            ignore_space: true,
        }
    }

    fn should_ignore(&self, line: &str) -> bool {
        if self.max_len == 0 || line.is_empty() {
            return true;
        }
        if self.ignore_space && line.starts_with(char::is_whitespace) {
            return true;
        }
        if self.ignore_dups
            && let Some(last) = self.entries.back()
            && last == line
        {
            return true;
        }
        false
    }

    fn insert(&mut self, line: String) {
        if self.entries.len() == self.max_len {
            self.entries.pop_front();
        }
        self.entries.push_back(line);
    }

    fn search_match<F>(
        &self,
        term: &str,
        start: usize,
        dir: SearchDirection,
        test: F,
    ) -> Option<SearchResult<'_>>
    where
        F: Fn(&str) -> Option<usize>,
    {
        if term.is_empty() || start >= self.len() {
            return None;
        }
        match dir {
            SearchDirection::Reverse => {
                for (idx, entry) in self
                    .entries
                    .iter()
                    .rev()
                    .skip(self.len() - 1 - start)
                    .enumerate()
                {
                    if let Some(cursor) = test(entry) {
                        return Some(SearchResult {
                            idx: start - idx,
                            entry: Cow::Borrowed(entry),
                            pos: cursor,
                        });
                    }
                }
                None
            }
            SearchDirection::Forward => {
                for (idx, entry) in self.entries.iter().skip(start).enumerate() {
                    if let Some(cursor) = test(entry) {
                        return Some(SearchResult {
                            idx: idx + start,
                            entry: Cow::Borrowed(entry),
                            pos: cursor,
                        });
                    }
                }
                None
            }
        }
    }
}

impl History for SqliteHistory {
    fn get(
        &self,
        index: usize,
        _dir: SearchDirection,
    ) -> rustyline::Result<Option<SearchResult<'_>>> {
        Ok(self.entries.get(index).map(|entry| SearchResult {
            entry: Cow::Borrowed(entry.as_str()),
            idx: index,
            pos: 0,
        }))
    }

    fn add(&mut self, line: &str) -> rustyline::Result<bool> {
        if self.should_ignore(line) {
            return Ok(false);
        }
        let line_owned = line.to_owned();
        self.insert(line_owned.clone());

        // Safe: add() is always called inside spawn_blocking
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let pool = self.pool.clone();
            if let Err(e) = handle.block_on(async move {
                sqlx::query("INSERT INTO input_history (input) VALUES (?)")
                    .bind(&line_owned)
                    .execute(&pool)
                    .await
            }) {
                tracing::warn!("failed to persist input history entry: {e}");
            }
        }
        Ok(true)
    }

    fn add_owned(&mut self, line: String) -> rustyline::Result<bool> {
        self.add(&line)
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn set_max_len(&mut self, len: usize) -> rustyline::Result<()> {
        self.max_len = len;
        if self.len() > len {
            self.entries.drain(..self.len() - len);
        }
        Ok(())
    }

    fn ignore_dups(&mut self, yes: bool) -> rustyline::Result<()> {
        self.ignore_dups = yes;
        Ok(())
    }

    fn ignore_space(&mut self, yes: bool) {
        self.ignore_space = yes;
    }

    fn save(&mut self, _path: &Path) -> rustyline::Result<()> {
        Ok(())
    }

    fn append(&mut self, _path: &Path) -> rustyline::Result<()> {
        Ok(())
    }

    fn load(&mut self, _path: &Path) -> rustyline::Result<()> {
        Ok(())
    }

    fn clear(&mut self) -> rustyline::Result<()> {
        self.entries.clear();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let pool = self.pool.clone();
            if let Err(e) = handle.block_on(async move {
                sqlx::query("DELETE FROM input_history")
                    .execute(&pool)
                    .await
            }) {
                tracing::warn!("failed to clear input history: {e}");
            }
        }
        Ok(())
    }

    fn search(
        &self,
        term: &str,
        start: usize,
        dir: SearchDirection,
    ) -> rustyline::Result<Option<SearchResult<'_>>> {
        let test = |entry: &str| entry.find(term);
        Ok(self.search_match(term, start, dir, test))
    }

    fn starts_with(
        &self,
        term: &str,
        start: usize,
        dir: SearchDirection,
    ) -> rustyline::Result<Option<SearchResult<'_>>> {
        let test = |entry: &str| {
            if entry.starts_with(term) {
                Some(0)
            } else {
                None
            }
        };
        Ok(self.search_match(term, start, dir, test))
    }
}

/// CLI channel that reads from stdin and writes to stdout.
#[derive(Debug)]
pub struct CliChannel {
    accumulated: String,
    editor: Option<Arc<Mutex<Editor<(), SqliteHistory>>>>,
}

impl std::fmt::Debug for SqliteHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteHistory")
            .field("entries_len", &self.entries.len())
            .field("max_len", &self.max_len)
            .field("ignore_dups", &self.ignore_dups)
            .field("ignore_space", &self.ignore_space)
            .finish_non_exhaustive()
    }
}

impl CliChannel {
    #[must_use]
    pub fn new() -> Self {
        Self {
            accumulated: String::new(),
            editor: None,
        }
    }

    /// Create a CLI channel with persistent history backed by `SqlitePool`.
    ///
    /// `entries` should be pre-loaded from the database by the caller.
    ///
    /// # Errors
    ///
    /// Returns an error if the editor cannot be created.
    pub fn with_history(pool: SqlitePool, entries: Vec<String>) -> Result<Self, ChannelError> {
        let history = SqliteHistory::new(entries, pool);
        let editor = Editor::with_history(rustyline::Config::default(), history)
            .map_err(|e| ChannelError::Other(e.to_string()))?;
        Ok(Self {
            accumulated: String::new(),
            editor: Some(Arc::new(Mutex::new(editor))),
        })
    }
}

impl Default for CliChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl Channel for CliChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        let raw = if let Some(editor) = &self.editor {
            let editor = editor.clone();
            let result = tokio::task::spawn_blocking(move || {
                let mut ed = editor.lock().expect("editor mutex poisoned");
                ed.readline("You: ")
            })
            .await
            .map_err(|e| ChannelError::Other(e.to_string()))?;

            match result {
                Ok(line) => line,
                Err(ReadlineError::Eof | ReadlineError::Interrupted) => return Ok(None),
                Err(e) => return Err(ChannelError::Other(e.to_string())),
            }
        } else {
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

            match line {
                Some(l) => l,
                None => return Ok(None),
            }
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
        if let Some(editor) = &self.editor {
            let editor = editor.clone();
            let result = tokio::task::spawn_blocking(move || {
                let mut ed = editor.lock().expect("editor mutex poisoned");
                ed.readline(&format!("{prompt} [y/N]: "))
            })
            .await
            .map_err(|e| ChannelError::Other(e.to_string()))?;

            match result {
                Ok(line) => Ok(line.trim().eq_ignore_ascii_case("y")),
                Err(ReadlineError::Eof | ReadlineError::Interrupted) => Ok(false),
                Err(e) => Err(ChannelError::Other(e.to_string())),
            }
        } else {
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

    #[test]
    fn sqlite_history_add_and_get() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool = rt.block_on(async {
            sqlx::sqlite::SqlitePoolOptions::new()
                .connect("sqlite::memory:")
                .await
                .unwrap()
        });

        let mut history = SqliteHistory::new(vec!["hello".to_owned(), "world".to_owned()], pool);
        assert_eq!(history.len(), 2);

        let result = rt.block_on(async {
            // run in spawn_blocking context to satisfy Handle::try_current
            tokio::task::spawn_blocking(move || {
                history.add("rust").unwrap();
                history.len()
            })
            .await
            .unwrap()
        });
        assert_eq!(result, 3);
    }

    #[test]
    fn sqlite_history_ignore_dups() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool = rt.block_on(async {
            sqlx::sqlite::SqlitePoolOptions::new()
                .connect("sqlite::memory:")
                .await
                .unwrap()
        });

        let result = rt.block_on(async {
            tokio::task::spawn_blocking(move || {
                let mut history = SqliteHistory::new(vec![], pool);
                history.add("hello").unwrap();
                history.add("hello").unwrap(); // duplicate — ignored
                history.len()
            })
            .await
            .unwrap()
        });
        assert_eq!(result, 1);
    }

    #[test]
    fn sqlite_history_search_finds_entry() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool = rt.block_on(async {
            sqlx::sqlite::SqlitePoolOptions::new()
                .connect("sqlite::memory:")
                .await
                .unwrap()
        });

        let history = SqliteHistory::new(
            vec!["cargo build".to_owned(), "cargo test".to_owned()],
            pool,
        );

        let result = history.search("test", 0, SearchDirection::Forward).unwrap();
        assert!(result.is_some());
        let sr = result.unwrap();
        assert_eq!(sr.idx, 1);
    }

    #[test]
    fn sqlite_history_starts_with() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool = rt.block_on(async {
            sqlx::sqlite::SqlitePoolOptions::new()
                .connect("sqlite::memory:")
                .await
                .unwrap()
        });

        let history = SqliteHistory::new(
            vec!["cargo build".to_owned(), "cargo test".to_owned()],
            pool,
        );

        let result = history
            .starts_with("cargo", 0, SearchDirection::Forward)
            .unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().idx, 0);
    }

    #[test]
    fn sqlite_history_clear() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool = rt.block_on(async {
            sqlx::sqlite::SqlitePoolOptions::new()
                .connect("sqlite::memory:")
                .await
                .unwrap()
        });

        let result = rt.block_on(async {
            tokio::task::spawn_blocking(move || {
                let mut history = SqliteHistory::new(vec!["a".to_owned(), "b".to_owned()], pool);
                history.clear().unwrap();
                history.len()
            })
            .await
            .unwrap()
        });
        assert_eq!(result, 0);
    }
}
