// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! CLI channel: stdin input and stdout output for interactive sessions.
//!
//! This module provides [`CliChannel`], the default channel used when Zeph
//! runs in CLI mode.  It handles two stdin modes transparently:
//!
//! * **TTY** — uses `line_editor::read_line` for readline-style interaction.
//! * **Piped** — reads lines from a `BufReader` in a dedicated OS thread.
//!
//! Input is always processed in a background task so that [`Channel::recv`] is
//! cancel-safe: dropping the future inside `tokio::select!` never loses
//! buffered messages.
//!
//! [`Channel::recv`]: zeph_core::channel::Channel::recv

use std::collections::VecDeque;
use std::io::{BufReader, IsTerminal};

use tokio::sync::mpsc;
use zeph_core::channel::{
    Attachment, AttachmentKind, Channel, ChannelError, ChannelMessage, ElicitationField,
    ElicitationFieldType, ElicitationRequest, ElicitationResponse,
};

use crate::line_editor::{self, ReadLineResult};

const STDIN_CHANNEL_CAPACITY: usize = 32;

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

/// Process a raw line from stdin: handle exit commands, empty-line logic,
/// `/image` commands. Returns `None` to continue the loop, `Some(msg)` to
/// send a message, or `Err(())` to break out of the loop.
async fn process_line(
    line: String,
    is_tty: bool,
    history: &mut Option<InputHistory>,
    pending_attachments: &mut Vec<Attachment>,
) -> Result<Option<ChannelMessage>, ()> {
    let trimmed = line.trim();

    match trimmed {
        "exit" | "quit" | "/exit" | "/quit" => return Err(()),
        "" => {
            // TTY: empty Enter ends session. Pipe: skip formatting blank lines.
            if is_tty {
                return Err(());
            }
            return Ok(None);
        }
        _ => {}
    }

    if let Some(h) = history {
        h.add(trimmed);
    }

    if let Some(path) = trimmed.strip_prefix("/image").map(str::trim) {
        if path.is_empty() {
            println!("Zeph: Usage: /image <path>");
            return Ok(None);
        }
        let path_owned = path.to_owned();
        let p = std::path::Path::new(&path_owned);
        if p.is_absolute() || p.components().any(|c| c == std::path::Component::ParentDir) {
            println!("Zeph: Invalid image path: path traversal not allowed");
            return Ok(None);
        }
        match tokio::fs::read(&path_owned).await {
            Err(e) => {
                println!("Zeph: Cannot read image {path_owned}: {e}");
            }
            Ok(data) => {
                let filename = std::path::Path::new(&path_owned)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_owned);
                let size = data.len();
                pending_attachments.push(Attachment {
                    kind: AttachmentKind::Image,
                    data,
                    filename,
                });
                println!("Zeph: Image attached: {path_owned} ({size} bytes). Send your message.");
            }
        }
        return Ok(None);
    }

    let attachments = std::mem::take(pending_attachments);
    Ok(Some(ChannelMessage {
        text: trimmed.to_string(),
        attachments,
    }))
}

/// Background stdin reader for TTY mode.
///
/// Spawns a `tokio::task::spawn_blocking` per line (using `line_editor::read_line`
/// which manages crossterm raw mode internally).
async fn run_tty_reader(mut history: Option<InputHistory>, tx: mpsc::Sender<ChannelMessage>) {
    let mut pending_attachments: Vec<Attachment> = Vec::new();

    loop {
        let entries: Vec<String> = history
            .as_ref()
            .map(|h| h.entries().iter().cloned().collect())
            .unwrap_or_default();

        let Ok(Ok(result)) =
            tokio::task::spawn_blocking(move || line_editor::read_line("You: ", &entries)).await
        else {
            break;
        };

        let line = match result {
            ReadLineResult::Interrupted | ReadLineResult::Eof => break,
            ReadLineResult::Line(l) => l,
        };

        match process_line(line, true, &mut history, &mut pending_attachments).await {
            Err(()) => break,
            Ok(None) => {}
            Ok(Some(msg)) => {
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Background stdin reader for piped (non-TTY) mode.
///
/// Runs a dedicated OS thread that owns a `BufReader<Stdin>` and calls
/// `line_editor::read_line_piped` in a loop. Results are shuttled back to an
/// async task via a tokio mpsc channel, avoiding repeated stdin locks.
async fn run_piped_reader(mut history: Option<InputHistory>, tx: mpsc::Sender<ChannelMessage>) {
    tracing::debug!("stdin is not a terminal, using piped input mode");

    let (line_tx, mut line_rx) = mpsc::channel::<Result<ReadLineResult, std::io::Error>>(1);

    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = BufReader::new(stdin);
        loop {
            let result = line_editor::read_line_piped(&mut reader);
            let is_eof = matches!(result, Ok(ReadLineResult::Eof));
            if line_tx.blocking_send(result).is_err() || is_eof {
                break;
            }
        }
    });

    let mut pending_attachments: Vec<Attachment> = Vec::new();

    loop {
        let Some(Ok(result)) = line_rx.recv().await else {
            break;
        };

        let line = match result {
            ReadLineResult::Interrupted | ReadLineResult::Eof => break,
            ReadLineResult::Line(l) => l,
        };

        match process_line(line, false, &mut history, &mut pending_attachments).await {
            Err(()) => break,
            Ok(None) => {}
            Ok(Some(msg)) => {
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Spawn a background task that reads stdin and sends processed messages through `tx`.
///
/// This makes `CliChannel::recv()` cancel-safe: messages buffered in the mpsc
/// channel are never dropped when the `recv()` future is cancelled by `tokio::select!`.
fn spawn_stdin_reader(
    is_tty: bool,
    history: Option<InputHistory>,
    tx: mpsc::Sender<ChannelMessage>,
) {
    tokio::spawn(async move {
        if is_tty {
            run_tty_reader(history, tx).await;
        } else {
            run_piped_reader(history, tx).await;
        }
    });
}

/// Pending configuration for the stdin reader background task.
///
/// The task is spawned lazily on the first call to `recv()`, ensuring that
/// `CliChannel::new()` is safe to call outside of a Tokio runtime context.
struct PendingReader {
    history: Option<InputHistory>,
    is_tty: bool,
}

impl std::fmt::Debug for PendingReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingReader")
            .field("is_tty", &self.is_tty)
            .finish_non_exhaustive()
    }
}

/// CLI channel that reads from stdin and writes to stdout.
///
/// Input is read in a background task (spawned lazily on the first [`Channel::recv`]
/// call), which makes `recv()` cancel-safe: dropping the future (e.g. inside a
/// `tokio::select!` branch) never discards buffered input — messages stay in the
/// internal [`mpsc`] channel and are returned on the next `recv()` call.
///
/// The channel automatically detects whether stdin is a TTY:
/// * **TTY mode** — uses `line_editor::read_line` with crossterm raw-mode for
///   readline-style editing (cursor movement, history navigation, `Ctrl-C`/`Ctrl-D`).
/// * **Piped mode** — spawns a dedicated OS thread that reads lines from a
///   [`BufReader`] and shuttles them through a tokio channel, avoiding repeated
///   stdin locks.
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_channels::CliChannel;
/// use zeph_core::channel::Channel;
///
/// # #[tokio::main]
/// # async fn example() {
/// let mut ch = CliChannel::new();
/// // Send a formatted reply to stdout.
/// ch.send("Hello from Zeph!").await.unwrap();
/// # }
/// ```
///
/// [`Channel::recv`]: zeph_core::channel::Channel::recv
/// [`BufReader`]: std::io::BufReader
#[derive(Debug)]
pub struct CliChannel {
    accumulated: String,
    /// Lazily-initialized receiver. `None` until `recv()` is called for the first time.
    input_rx: Option<mpsc::Receiver<ChannelMessage>>,
    /// Pending configuration consumed when the background task is first spawned.
    pending: Option<PendingReader>,
}

impl CliChannel {
    /// Create a new CLI channel without persistent history.
    ///
    /// This is safe to call outside of a Tokio runtime; the background stdin
    /// reader task is not spawned until the first [`Channel::recv`] call.
    ///
    /// [`Channel::recv`]: zeph_core::channel::Channel::recv
    #[must_use]
    pub fn new() -> Self {
        let is_tty = std::io::stdin().is_terminal();
        Self {
            accumulated: String::new(),
            input_rx: None,
            pending: Some(PendingReader {
                history: None,
                is_tty,
            }),
        }
    }

    /// Create a CLI channel with persistent input history.
    ///
    /// `entries` is a pre-loaded history list (e.g. loaded from `SQLite` on
    /// startup).  `persist_fn` is called for each newly submitted entry so the
    /// caller can persist it (e.g. via `SqliteStore::save_input_entry`).
    ///
    /// Duplicate consecutive entries are silently ignored; empty lines are never
    /// added to the history.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use zeph_channels::CliChannel;
    ///
    /// let previous: Vec<String> = vec!["ls -la".into(), "cargo build".into()];
    /// let ch = CliChannel::with_history(previous, |entry| {
    ///     // Persist `entry` to your storage layer.
    ///     eprintln!("saving: {entry}");
    /// });
    /// ```
    #[must_use]
    pub fn with_history(entries: Vec<String>, persist_fn: impl Fn(&str) + Send + 'static) -> Self {
        let is_tty = std::io::stdin().is_terminal();
        let history = InputHistory::new(entries, Box::new(persist_fn));
        Self {
            accumulated: String::new(),
            input_rx: None,
            pending: Some(PendingReader {
                history: Some(history),
                is_tty,
            }),
        }
    }

    /// Ensure the background stdin reader is running and return a mutable
    /// reference to the receiver. Called from within an async context only.
    fn ensure_reader(&mut self) -> &mut mpsc::Receiver<ChannelMessage> {
        if self.input_rx.is_none() {
            let pending = self
                .pending
                .take()
                .expect("PendingReader consumed before input_rx was set");
            let (tx, rx) = mpsc::channel(STDIN_CHANNEL_CAPACITY);
            spawn_stdin_reader(pending.is_tty, pending.history, tx);
            self.input_rx = Some(rx);
        }
        self.input_rx.as_mut().expect("input_rx set above")
    }
}

impl Default for CliChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl Channel for CliChannel {
    /// Receive the next user message.
    ///
    /// This method is cancel-safe: dropping the future does not discard any
    /// buffered input. The background stdin reader task buffers messages in an
    /// mpsc channel; they remain available on the next `recv()` call.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channel.cli.recv", skip_all, fields(msg_len = tracing::field::Empty))
    )]
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        Ok(self.ensure_reader().recv().await)
    }

    /// Write a complete agent reply to stdout.
    ///
    /// The message is prefixed with `"Zeph: "` and followed by a newline.
    /// Use [`send_chunk`] / [`flush_chunks`] for streaming output instead.
    ///
    /// # Errors
    ///
    /// Always returns `Ok(())` — stdout writes do not produce recoverable
    /// errors in this adapter.
    ///
    /// [`send_chunk`]: CliChannel::send_chunk
    /// [`flush_chunks`]: CliChannel::flush_chunks
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channel.cli.send", skip_all, fields(msg_len = %text.len()))
    )]
    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        println!("Zeph: {text}");
        Ok(())
    }

    /// Write a streaming chunk to stdout and accumulate it internally.
    ///
    /// Chunks are printed without a trailing newline so that the response
    /// streams character-by-character.  Call [`flush_chunks`] when the stream
    /// is complete to emit the final newline and clear the internal buffer.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the stdout flush fails.
    ///
    /// [`flush_chunks`]: CliChannel::flush_chunks
    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        use std::io::{Write, stdout};
        print!("{chunk}");
        stdout().flush()?;
        self.accumulated.push_str(chunk);
        Ok(())
    }

    /// Finalise a streamed response by printing a trailing newline.
    ///
    /// Clears the internal accumulation buffer so the channel is ready for the
    /// next response.
    ///
    /// # Errors
    ///
    /// Always returns `Ok(())`.
    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        println!();
        self.accumulated.clear();
        Ok(())
    }

    /// Prompt the user for a yes/no confirmation on stdin.
    ///
    /// In non-interactive (piped) mode the method auto-declines and returns
    /// `Ok(false)` without blocking.  In TTY mode it reads one line and returns
    /// `true` only when the user types `y` or `Y`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if spawning the blocking task fails or if the underlying
    /// readline call returns an I/O error.
    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        if !std::io::stdin().is_terminal() {
            tracing::debug!("non-interactive stdin, auto-declining confirmation");
            return Ok(false);
        }
        let prompt = format!("{prompt} [y/N]: ");
        let result = tokio::task::spawn_blocking(move || line_editor::read_line(&prompt, &[]))
            .await
            .map_err(ChannelError::other)?
            .map_err(ChannelError::Io)?;

        match result {
            ReadLineResult::Line(line) => Ok(line.trim().eq_ignore_ascii_case("y")),
            ReadLineResult::Interrupted | ReadLineResult::Eof => Ok(false),
        }
    }

    /// Collect structured input from the user on behalf of an MCP server.
    ///
    /// Prompts the user for each field in `request.fields` sequentially.  In
    /// non-interactive (piped) mode the method logs a warning and auto-declines
    /// without blocking.
    ///
    /// Field values are coerced to the declared [`ElicitationFieldType`].  If a
    /// value cannot be coerced the method returns
    /// [`ElicitationResponse::Declined`] immediately.  `Ctrl-C` or `Ctrl-D`
    /// returns [`ElicitationResponse::Cancelled`].
    ///
    /// # Errors
    ///
    /// Returns `Err` if spawning the blocking task fails or if the underlying
    /// readline call returns an I/O error.
    ///
    /// [`ElicitationFieldType`]: zeph_core::channel::ElicitationFieldType
    /// [`ElicitationResponse::Declined`]: zeph_core::channel::ElicitationResponse::Declined
    /// [`ElicitationResponse::Cancelled`]: zeph_core::channel::ElicitationResponse::Cancelled
    async fn elicit(
        &mut self,
        request: ElicitationRequest,
    ) -> Result<ElicitationResponse, ChannelError> {
        if !std::io::stdin().is_terminal() {
            tracing::warn!(
                server = request.server_name,
                "non-interactive stdin, auto-declining elicitation"
            );
            return Ok(ElicitationResponse::Declined);
        }

        println!(
            "\n[MCP server '{}' is requesting input]",
            request.server_name
        );
        println!("{}", request.message);

        let mut values = serde_json::Map::new();
        for field in &request.fields {
            let prompt = build_field_prompt(field);
            let field_name = field.name.clone();
            let result = tokio::task::spawn_blocking(move || line_editor::read_line(&prompt, &[]))
                .await
                .map_err(ChannelError::other)?
                .map_err(ChannelError::Io)?;

            match result {
                ReadLineResult::Line(line) => {
                    let trimmed = line.trim().to_owned();
                    if let Some(value) = coerce_field_value(&trimmed, &field.field_type) {
                        values.insert(field_name, value);
                    } else {
                        println!(
                            "Invalid input for '{}' (expected {:?}), declining.",
                            field_name, field.field_type
                        );
                        return Ok(ElicitationResponse::Declined);
                    }
                }
                ReadLineResult::Interrupted | ReadLineResult::Eof => {
                    return Ok(ElicitationResponse::Cancelled);
                }
            }
        }

        Ok(ElicitationResponse::Accepted(serde_json::Value::Object(
            values,
        )))
    }
}

/// Build a human-readable prompt string for a single elicitation field.
///
/// The prompt includes the field name, an optional description in parentheses,
/// and a type hint (e.g. `[true/false]`, `[number]`, or the list of allowed
/// enum values separated by `/`).
fn build_field_prompt(field: &ElicitationField) -> String {
    let type_hint = match &field.field_type {
        ElicitationFieldType::Boolean => " [true/false]",
        ElicitationFieldType::Integer | ElicitationFieldType::Number => " [number]",
        ElicitationFieldType::Enum(opts) if !opts.is_empty() => {
            // Build hint dynamically below
            return format!(
                "{}{}: ",
                field.name,
                field
                    .description
                    .as_deref()
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default()
            ) + &format!("[{}]: ", opts.join("/"));
        }
        _ => "",
    };
    format!(
        "{}{}{}",
        field.name,
        field
            .description
            .as_deref()
            .map(|d| format!(" ({d})"))
            .unwrap_or_default(),
        if type_hint.is_empty() {
            ": ".to_owned()
        } else {
            format!("{type_hint}: ")
        }
    )
}

/// Coerce a raw user-input string into the JSON type required by the field.
/// Returns `None` if the input cannot be converted to the declared type.
fn coerce_field_value(raw: &str, field_type: &ElicitationFieldType) -> Option<serde_json::Value> {
    match field_type {
        ElicitationFieldType::String => Some(serde_json::Value::String(raw.to_owned())),
        ElicitationFieldType::Boolean => match raw.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(serde_json::Value::Bool(true)),
            "false" | "no" | "0" => Some(serde_json::Value::Bool(false)),
            _ => None,
        },
        ElicitationFieldType::Integer => raw
            .parse::<i64>()
            .ok()
            .map(|n| serde_json::Value::Number(n.into())),
        ElicitationFieldType::Number => raw
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(serde_json::Value::Number),
        ElicitationFieldType::Enum(opts) => {
            if opts.iter().any(|o| o == raw) {
                Some(serde_json::Value::String(raw.to_owned()))
            } else {
                None
            }
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
    async fn cli_channel_flush_chunks_clears_buffer() {
        let mut ch = CliChannel::new();
        ch.send_chunk("test").await.unwrap();
        ch.flush_chunks().await.unwrap();
        assert!(ch.accumulated.is_empty());
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
    async fn image_command_valid_file_stores_in_pending() {
        use std::io::Write;

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let image_bytes = b"\x89PNG\r\n\x1a\nfake-image-data";
        tmp.write_all(image_bytes).unwrap();
        tmp.flush().unwrap();

        let path = tmp.path().to_str().unwrap().to_owned();

        let data = tokio::fs::read(&path).await.unwrap();
        let filename = std::path::Path::new(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_owned);

        let mut pending_attachments: Vec<Attachment> = Vec::new();
        pending_attachments.push(Attachment {
            kind: AttachmentKind::Image,
            data: data.clone(),
            filename,
        });

        assert_eq!(pending_attachments.len(), 1);
        assert_eq!(pending_attachments[0].data, image_bytes);
        assert_eq!(pending_attachments[0].kind, AttachmentKind::Image);

        let taken = std::mem::take(&mut pending_attachments);
        assert!(pending_attachments.is_empty());
        assert_eq!(taken.len(), 1);
    }

    #[tokio::test]
    async fn image_command_missing_file_is_handled_gracefully() {
        let result = tokio::fs::read("/nonexistent/path/image.png").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn image_command_empty_args_detected() {
        let trimmed = "/image";
        let arg = trimmed.strip_prefix("/image").map_or("", str::trim);
        assert!(arg.is_empty());

        let trimmed_space = "/image   ";
        let arg_space = trimmed_space.strip_prefix("/image").map_or("", str::trim);
        assert!(arg_space.is_empty());
    }

    #[test]
    fn cli_channel_new_has_empty_accumulated() {
        let ch = CliChannel::new();
        assert!(ch.accumulated.is_empty());
    }

    #[test]
    fn cli_channel_with_history_constructs_ok() {
        let ch = CliChannel::with_history(vec![], |_| {});
        assert!(ch.accumulated.is_empty());
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

    /// Verify that `recv()` is cancel-safe: dropping the future does not discard
    /// buffered input. This is the regression test for the `tokio::select!` race
    /// that caused stdin input to be silently lost when a reload branch won.
    #[tokio::test]
    async fn recv_is_cancel_safe_via_mpsc_buffer() {
        // Create a direct mpsc pair to simulate the background reader.
        let (tx, rx) = mpsc::channel::<ChannelMessage>(32);
        let mut ch = CliChannel {
            accumulated: String::new(),
            input_rx: Some(rx),
            pending: None,
        };

        // Pre-fill the channel with a message (simulates background reader
        // having already buffered input before select! cancellation).
        tx.send(ChannelMessage {
            text: "hello".to_string(),
            attachments: vec![],
        })
        .await
        .unwrap();

        // Simulate select! cancellation: drop the recv() future without polling it.
        // This models the scenario where a reload branch wins the select! race.
        drop(ch.recv());

        // The buffered message must still be available on the next recv() call.
        let result = ch.recv().await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().text, "hello");
    }

    #[tokio::test]
    async fn image_command_absolute_path_is_rejected() {
        let mut pending: Vec<Attachment> = Vec::new();
        let mut history = Some(InputHistory::new(vec![], Box::new(|_| {})));
        let result = process_line(
            "/image /etc/passwd".to_owned(),
            false,
            &mut history,
            &mut pending,
        )
        .await;
        assert!(matches!(result, Ok(None)));
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn image_command_parent_dir_traversal_is_rejected() {
        let mut pending: Vec<Attachment> = Vec::new();
        let mut history = Some(InputHistory::new(vec![], Box::new(|_| {})));
        let result = process_line(
            "/image ../../../etc/passwd".to_owned(),
            false,
            &mut history,
            &mut pending,
        )
        .await;
        assert!(matches!(result, Ok(None)));
        assert!(pending.is_empty());
    }
}
