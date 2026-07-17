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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{Notify, mpsc};
use zeph_common::path_guard::{PathRejection, classify_relative_path};
use zeph_core::channel::{
    Attachment, AttachmentKind, Channel, ChannelError, ChannelMessage, ElicitationField,
    ElicitationFieldType, ElicitationRequest, ElicitationResponse,
};

use crate::line_editor::{self, ReadLineResult};

/// Coordinates exclusive terminal access between the persistent background
/// chat-input reader ([`run_tty_reader`]) and one-shot `elicit()`/`confirm()`
/// prompts.
///
/// Both readers ultimately call into crossterm's process-wide
/// `event::read()`, which has no concept of "which caller should receive
/// this event" — without this coordination, keystrokes typed during an
/// elicitation/confirmation prompt can be stolen by the background
/// chat-input reader instead (#6398).
#[derive(Debug)]
struct StdinCoordination {
    /// Set while an `elicit()`/`confirm()` prompt owns the terminal. Polled by
    /// `run_tty_reader`'s interruptible read loop.
    elicit_active: AtomicBool,
    /// Notified when `elicit_active` transitions back to `false`, so the
    /// paused background reader wakes without busy-polling.
    resume: Notify,
    /// Notified by `run_tty_reader` once it has observed `elicit_active`,
    /// bumped [`Self::parked_generation`], and is about to park on `resume`
    /// — i.e. it has genuinely stopped touching stdin. Paired with
    /// `parked_generation` because a bare `Notify` permit can be stored by an
    /// ack fired with no waiter (e.g. after `ElicitGuard::acquire()` already
    /// gave up via [`ACK_HANDSHAKE_TIMEOUT`]) and then be wrongly consumed by
    /// a *later* `acquire()` call that never actually waited for its own
    /// reader parking — `acquire()` must check the generation to reject such
    /// a stale permit (#6404).
    ack: Notify,
    /// Incremented by `run_tty_reader` immediately before each `ack.notify_one()`
    /// call, i.e. once per genuine park. `ElicitGuard::acquire()` captures the
    /// value at entry and only accepts an ack whose observed generation is
    /// strictly greater — defeating stale permits left over from an earlier,
    /// unrelated park (see `ack` doc above).
    parked_generation: AtomicU64,
}

impl StdinCoordination {
    fn new() -> Self {
        Self {
            elicit_active: AtomicBool::new(false),
            resume: Notify::new(),
            ack: Notify::new(),
            parked_generation: AtomicU64::new(0),
        }
    }
}

/// Bound on how long [`ElicitGuard::acquire`] waits for `run_tty_reader`'s ack
/// handshake before proceeding anyway.
///
/// The background reader's `event::poll` cycle is at most 50ms, so a genuine
/// ack normally arrives well within this bound. The timeout exists only to
/// guard against a reader that will never ack: it was never spawned (e.g.
/// `elicit()`/`confirm()` called before the first [`Channel::recv`]), or it
/// already exited (Ctrl-D/EOF). In either case, proceeding without the
/// handshake — the pre-#6404 behaviour — is preferable to hanging forever.
///
/// [`Channel::recv`]: zeph_core::channel::Channel::recv
const ACK_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(200);

/// RAII guard granting an `elicit()`/`confirm()` prompt exclusive terminal
/// access for its lifetime.
///
/// Acquiring sets [`StdinCoordination::elicit_active`], constructs the guard
/// immediately (so `Drop` is armed even if the caller is cancelled before
/// acquisition finishes), and then awaits the reader's ack — rejecting any
/// stale permit left over from an earlier, unrelated park via
/// [`StdinCoordination::parked_generation`] — bounded by
/// [`ACK_HANDSHAKE_TIMEOUT`] so the prompt only proceeds once `run_tty_reader`
/// has genuinely stopped touching stdin, or the bound is exceeded. Dropping —
/// on any exit path, including cancellation of the `acquire()` future itself
/// — clears the flag and wakes the paused background reader via
/// [`StdinCoordination::resume`].
struct ElicitGuard<'a> {
    coord: &'a StdinCoordination,
}

impl<'a> ElicitGuard<'a> {
    async fn acquire(coord: &'a StdinCoordination) -> Self {
        let start_generation = coord.parked_generation.load(Ordering::Acquire);
        coord.elicit_active.store(true, Ordering::Release);
        // Constructed before the ack wait so `Drop` clears `elicit_active`
        // even if this future is dropped mid-await (#6404 S2).
        let guard = Self { coord };

        let deadline = tokio::time::Instant::now() + ACK_HANDSHAKE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                tracing::debug!(
                    "ack handshake timed out waiting for the background stdin reader to park; \
                    proceeding without it (reader may not be running)"
                );
                break;
            }
            if tokio::time::timeout(remaining, coord.ack.notified())
                .await
                .is_err()
            {
                tracing::debug!(
                    "ack handshake timed out waiting for the background stdin reader to park; \
                    proceeding without it (reader may not be running)"
                );
                break;
            }
            // A woken `notified()` can be a stale permit from a park that
            // predates this acquire() (#6404 S1) — only a generation strictly
            // newer than the one observed at entry proves the reader parked
            // *for this request*. A stale wakeup loops back and keeps
            // waiting within the same overall deadline.
            if coord.parked_generation.load(Ordering::Acquire) > start_generation {
                break;
            }
        }
        guard
    }
}

impl Drop for ElicitGuard<'_> {
    fn drop(&mut self) {
        self.coord.elicit_active.store(false, Ordering::Release);
        self.coord.resume.notify_one();
    }
}

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

/// Format the `/image` rejection message for a [`PathRejection`], or `None` when the
/// path is allowed.
///
/// Extracted as a pure function (rather than inlined `println!` calls) so the exact
/// message text is directly unit-testable without capturing stdout.
fn image_path_rejection_message(rejection: PathRejection) -> Option<&'static str> {
    match rejection {
        PathRejection::Allowed => None,
        PathRejection::Absolute => Some(
            "Zeph: Invalid image path: absolute paths are not supported, use a path \
            relative to the working directory",
        ),
        PathRejection::Traversal => {
            Some("Zeph: Invalid image path: path traversal ('..') is not allowed")
        }
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
        if let Some(msg) = image_path_rejection_message(classify_relative_path(&path_owned)) {
            println!("{msg}");
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
        is_guest_context: false,
        is_from_bot: false,
        owner_key: None,
    }))
}

/// Background stdin reader for TTY mode.
///
/// Spawns a `tokio::task::spawn_blocking` per line (using
/// `line_editor::read_line_yieldable`, which manages crossterm raw mode
/// internally). Before each read attempt, waits for `coord.elicit_active` to
/// clear so that an active `elicit()`/`confirm()` prompt has exclusive
/// terminal access — see [`StdinCoordination`].
async fn run_tty_reader(
    mut history: Option<InputHistory>,
    tx: mpsc::Sender<ChannelMessage>,
    coord: Arc<StdinCoordination>,
) {
    let mut pending_attachments: Vec<Attachment> = Vec::new();

    loop {
        while coord.elicit_active.load(Ordering::Acquire) {
            // Reached only once this reader has stopped calling
            // `event::poll`/`event::read()` for the current line (either it
            // never started this iteration, or the prior `spawn_blocking`
            // call already returned `Yielded`) — so acking here is exactly
            // the "genuinely stopped touching stdin" signal `ElicitGuard::
            // acquire()` waits for (#6404). The generation bump happens
            // before the notify so a waiter that wakes on this ack always
            // observes a generation newer than the one it captured at entry
            // (#6404 S1 — defeats stale-permit consumption by a later,
            // unrelated `acquire()` call).
            coord.parked_generation.fetch_add(1, Ordering::Release);
            coord.ack.notify_one();
            coord.resume.notified().await;
        }

        let entries: Vec<String> = history
            .as_ref()
            .map(|h| h.entries().iter().cloned().collect())
            .unwrap_or_default();

        crate::terminal_title::set_action_required("zeph");
        // NOTE: raw spawn_blocking is correct here — this is interactive terminal I/O (crossterm
        // raw mode), not a CPU-bound agent task. Routing through task_supervisor's semaphore
        // would starve the UI when 8 agent tasks are in-flight.
        let coord_for_blocking = Arc::clone(&coord);
        let Ok(Ok(result)) = tokio::task::spawn_blocking(move || {
            line_editor::read_line_yieldable("You: ", &entries, &coord_for_blocking.elicit_active)
        })
        .await
        else {
            break;
        };
        crate::terminal_title::clear_action_required("zeph");

        let line = match result {
            // The wait-loop above will now block until elicit()/confirm() releases the terminal.
            ReadLineResult::Yielded => continue,
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
            // `read_line_piped` never yields; the reader loop above only calls it. Handled
            // gracefully (not `unreachable!()`) so a future refactor accidentally routing this
            // path through the yieldable variant degrades instead of panicking.
            ReadLineResult::Yielded => continue,
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
///
/// # spec-039 exception
///
/// This site intentionally uses `tokio::spawn` directly rather than `TaskSupervisor`:
/// the stdin reader is an interactive I/O task with process lifetime that cannot be
/// restarted (stdin is a singleton OS resource) and no `TaskSupervisor` reaches
/// `CliChannel` by design — the CLI path bypasses the channel builder's supervisor
/// plumbing. This mirrors the existing `spawn_blocking` readline exceptions at
/// cli.rs:161/463/518. A panic here terminates the process, which is correct behaviour.
fn spawn_stdin_reader(
    is_tty: bool,
    history: Option<InputHistory>,
    tx: mpsc::Sender<ChannelMessage>,
    coord: Arc<StdinCoordination>,
) {
    tokio::spawn(async move {
        if is_tty {
            run_tty_reader(history, tx, coord).await;
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
    /// Shared terminal-access coordination between the background chat-input
    /// reader and `elicit()`/`confirm()` prompts. See [`StdinCoordination`].
    stdin_coord: Arc<StdinCoordination>,
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
            stdin_coord: Arc::new(StdinCoordination::new()),
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
            stdin_coord: Arc::new(StdinCoordination::new()),
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
            spawn_stdin_reader(
                pending.is_tty,
                pending.history,
                tx,
                Arc::clone(&self.stdin_coord),
            );
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
    #[tracing::instrument(name = "channels.cli.recv", skip_all, fields(msg_len = tracing::field::Empty))]
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
    #[tracing::instrument(name = "channels.cli.send", skip_all, fields(msg_len = %text.len()))]
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
    #[tracing::instrument(name = "channels.cli.send_chunk", skip_all, fields(chunk_len = chunk.len()))]
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
    #[tracing::instrument(name = "channels.cli.flush_chunks", skip_all)]
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
    #[tracing::instrument(name = "channels.cli.confirm", skip_all)]
    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        if !std::io::stdin().is_terminal() {
            tracing::debug!("non-interactive stdin, auto-declining confirmation");
            return Ok(false);
        }
        let _guard = ElicitGuard::acquire(&self.stdin_coord).await;
        let prompt = format!("{prompt} [y/N]: ");
        // NOTE: raw spawn_blocking is intentional — interactive terminal readline; not an agent
        // task, so the task_supervisor semaphore does not apply.
        let result = tokio::task::spawn_blocking(move || line_editor::read_line(&prompt, &[]))
            .await
            .map_err(ChannelError::other)?
            .map_err(ChannelError::Io)?;

        match result {
            ReadLineResult::Line(line) => Ok(line.trim().eq_ignore_ascii_case("y")),
            // `read_line` (non-yieldable) never actually returns `Yielded`; folded in here
            // (rather than a separate `unreachable!()` arm) so a future refactor routing this
            // call through the yieldable variant degrades to a declined confirmation instead of
            // panicking.
            ReadLineResult::Interrupted | ReadLineResult::Eof | ReadLineResult::Yielded => {
                Ok(false)
            }
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
    #[tracing::instrument(name = "channels.cli.elicit", skip_all, fields(server = %request.server_name))]
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

        let _guard = ElicitGuard::acquire(&self.stdin_coord).await;

        println!(
            "\n[MCP server '{}' is requesting input]",
            request.server_name
        );
        println!("{}", request.message);

        let mut values = serde_json::Map::new();
        for field in &request.fields {
            let prompt = build_field_prompt(field);
            let field_name = field.name.clone();
            // NOTE: raw spawn_blocking is intentional — interactive terminal readline; not an
            // agent task, so the task_supervisor semaphore does not apply.
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
                // `read_line` (non-yieldable) never actually returns `Yielded`; folded in here
                // (rather than a separate `unreachable!()` arm) so a future refactor routing
                // this call through the yieldable variant degrades to a cancelled elicitation
                // instead of panicking.
                ReadLineResult::Interrupted | ReadLineResult::Eof | ReadLineResult::Yielded => {
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
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn cli_channel_default() {
        let ch = CliChannel::default();
        let _ = format!("{ch:?}");
    }

    /// Spawns a task that satisfies a freshly-started `ElicitGuard::acquire()`
    /// call with a genuinely fresh ack (bumps the generation, then notifies),
    /// for tests that only care about flag/resume semantics and don't want to
    /// pay `ACK_HANDSHAKE_TIMEOUT`. Must be called immediately before
    /// `.await`ing the `acquire()` call it's meant to satisfy — `tokio::spawn`
    /// only enqueues the task, so it runs at `acquire()`'s first internal
    /// suspension point (after `acquire()` has already captured its starting
    /// generation), never before.
    fn arm_ack_once(coord: &Arc<StdinCoordination>) {
        let coord = Arc::clone(coord);
        tokio::spawn(async move {
            coord.parked_generation.fetch_add(1, Ordering::Release);
            coord.ack.notify_one();
        });
    }

    #[tokio::test]
    async fn elicit_guard_acquire_sets_flag_true() {
        let coord = Arc::new(StdinCoordination::new());
        assert!(!coord.elicit_active.load(Ordering::Acquire));
        // No reader task is running in this test; arm a fresh ack so
        // `acquire()` resolves immediately instead of via its timeout
        // fallback — this test is about the flag, not the handshake.
        arm_ack_once(&coord);
        let _guard = ElicitGuard::acquire(&coord).await;
        assert!(coord.elicit_active.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn elicit_guard_drop_clears_flag() {
        let coord = Arc::new(StdinCoordination::new());
        arm_ack_once(&coord);
        {
            let _guard = ElicitGuard::acquire(&coord).await;
            assert!(coord.elicit_active.load(Ordering::Acquire));
        }
        assert!(!coord.elicit_active.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn elicit_guard_drop_wakes_a_notified_waiter() {
        let coord = Arc::new(StdinCoordination::new());
        arm_ack_once(&coord);
        let guard = ElicitGuard::acquire(&coord).await;

        let waiter_coord = Arc::clone(&coord);
        let waiter = tokio::spawn(async move {
            waiter_coord.resume.notified().await;
        });

        // Give the waiter a chance to register before the guard drops.
        tokio::task::yield_now().await;
        drop(guard);

        tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("waiter should wake within timeout")
            .expect("waiter task should not panic");
    }

    /// Regression test for #6404: under the old poll-based approach,
    /// `ElicitGuard::acquire()` returned as soon as it set `elicit_active`,
    /// with no guarantee the background reader had actually stopped touching
    /// stdin — only a ~50ms assumption. This proves `acquire()` now blocks
    /// until the reader's ack genuinely fires, not just until some elapsed
    /// delay: the ack is deliberately delayed, and `acquire()` must not
    /// return before it lands.
    #[tokio::test]
    async fn elicit_guard_acquire_awaits_reader_ack_handshake() {
        let coord = Arc::new(StdinCoordination::new());
        let ack_fired = Arc::new(AtomicBool::new(false));

        let acking_coord = Arc::clone(&coord);
        let acking_flag = Arc::clone(&ack_fired);
        tokio::spawn(async move {
            // Simulates `run_tty_reader` still mid-`event::poll` when the
            // flag is set, only acking once it has genuinely parked.
            tokio::time::sleep(Duration::from_millis(30)).await;
            acking_flag.store(true, Ordering::Release);
            acking_coord
                .parked_generation
                .fetch_add(1, Ordering::Release);
            acking_coord.ack.notify_one();
        });

        let guard = ElicitGuard::acquire(&coord).await;
        assert!(
            ack_fired.load(Ordering::Acquire),
            "acquire() must not return before observing the reader's ack"
        );
        drop(guard);
    }

    /// Guards against reintroducing a hang: if `run_tty_reader` was never
    /// spawned (e.g. `elicit()`/`confirm()` called before the first `recv()`)
    /// or already exited, the ack `Notify` never fires. `acquire()` must fall
    /// back to proceeding after `ACK_HANDSHAKE_TIMEOUT` rather than blocking
    /// forever (#6404).
    #[tokio::test]
    async fn elicit_guard_acquire_does_not_hang_when_reader_never_acks() {
        let coord = StdinCoordination::new();
        let guard = tokio::time::timeout(Duration::from_secs(1), ElicitGuard::acquire(&coord))
            .await
            .expect("acquire() must not hang indefinitely when no reader ever acks");
        drop(guard);
    }

    /// Regression test for #6404 S1 (impl-critic finding): a stale `ack`
    /// permit left over from an earlier, *timed-out* `acquire()` must not
    /// satisfy a later, unrelated `acquire()` call on the same `coord`.
    /// `tokio::sync::Notify::notify_one()` stores a permit when fired with no
    /// current waiter; without the `parked_generation` check, the second
    /// `acquire()` would instantly consume that leftover permit and return
    /// believing the reader had parked for its own request — silently
    /// reintroducing the original ~50ms race for every prompt that
    /// immediately follows a timed-out one.
    #[tokio::test]
    async fn elicit_guard_acquire_rejects_stale_permit_from_prior_timed_out_acquire() {
        let coord = Arc::new(StdinCoordination::new());

        // First acquire(): nothing acks it, so it must time out and proceed
        // via the fallback path.
        let guard1 = tokio::time::timeout(Duration::from_secs(1), ElicitGuard::acquire(&coord))
            .await
            .expect("first acquire() must not hang");
        drop(guard1);

        // Simulate the reader "catching up" after the fact: it parks and
        // fires an ack with nobody currently waiting on it — the resulting
        // permit must NOT satisfy the next acquire() below.
        coord.parked_generation.fetch_add(1, Ordering::Release);
        coord.ack.notify_one();

        // Second acquire() must ignore that stale permit and wait for a
        // genuinely fresh ack tied to its own request.
        let acking_coord = Arc::clone(&coord);
        let ack_fired = Arc::new(AtomicBool::new(false));
        let acking_flag = Arc::clone(&ack_fired);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            acking_flag.store(true, Ordering::Release);
            acking_coord
                .parked_generation
                .fetch_add(1, Ordering::Release);
            acking_coord.ack.notify_one();
        });

        let guard2 = ElicitGuard::acquire(&coord).await;
        assert!(
            ack_fired.load(Ordering::Acquire),
            "acquire() must not be satisfied by a stale permit left over from an earlier, \
            unrelated park — it must wait for a fresh ack tied to its own request"
        );
        drop(guard2);
    }

    /// Regression test for #6404 S2 (impl-critic finding): dropping the
    /// `acquire()` future mid-await (task cancellation, a `select!` loser, an
    /// aborted `JoinHandle`) must still clear `elicit_active` via the guard's
    /// `Drop`. `Drop` only runs once `Self { coord }` has actually been
    /// constructed — regressing the order so the flag is set *before* the
    /// guard exists would strand it `true` forever whenever `acquire()` is
    /// cancelled during the ack wait, permanently parking `run_tty_reader`
    /// and killing all subsequent chat stdin input.
    #[tokio::test]
    async fn elicit_guard_acquire_cancelled_mid_await_clears_flag() {
        let coord = Arc::new(StdinCoordination::new());
        assert!(!coord.elicit_active.load(Ordering::Acquire));

        // Race acquire() (which nothing ever acks, so it would otherwise sit
        // in its internal loop for up to ACK_HANDSHAKE_TIMEOUT) against an
        // immediate `yield_now()`. `select!` polls every branch each round;
        // `yield_now()` always resolves on its second poll, while acquire()
        // is still pending (no ack fires and 200ms hasn't elapsed) — so the
        // `yield_now()` branch wins deterministically and acquire()'s future
        // is dropped mid-await.
        tokio::select! {
            _ = ElicitGuard::acquire(&coord) => {
                panic!("acquire() must not resolve — nothing ever fires its ack");
            }
            () = tokio::task::yield_now() => {}
        }

        assert!(
            !coord.elicit_active.load(Ordering::Acquire),
            "dropping acquire() mid-await must still clear elicit_active via the guard's Drop"
        );
    }

    /// Mirrors `run_tty_reader`'s wait loop exactly (including the generation
    /// bump and ack fired just before parking), so this test exercises the
    /// actual consumer-side coordination pattern (not just `ElicitGuard` in
    /// isolation).
    async fn wait_for_resume(coord: &StdinCoordination) {
        while coord.elicit_active.load(Ordering::Acquire) {
            coord.parked_generation.fetch_add(1, Ordering::Release);
            coord.ack.notify_one();
            coord.resume.notified().await;
        }
    }

    #[tokio::test]
    async fn stdin_coord_wait_loop_blocks_while_guard_held_then_resumes_on_drop() {
        let coord = Arc::new(StdinCoordination::new());
        arm_ack_once(&coord);
        let guard = ElicitGuard::acquire(&coord).await;

        let waiter_coord = Arc::clone(&coord);
        let waiter = tokio::spawn(async move { wait_for_resume(&waiter_coord).await });

        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !waiter.is_finished(),
            "wait loop must stay blocked while the guard is held"
        );

        drop(guard);

        tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("wait loop should exit within timeout after guard drop")
            .expect("waiter task should not panic");
    }

    #[tokio::test]
    async fn stdin_coord_wait_loop_ignores_spurious_notify_while_flag_still_true() {
        let coord = Arc::new(StdinCoordination::new());
        coord.elicit_active.store(true, Ordering::Release);

        let waiter_coord = Arc::clone(&coord);
        let waiter = tokio::spawn(async move { wait_for_resume(&waiter_coord).await });

        tokio::task::yield_now().await;

        // A notify while the flag is still true must not let the waiter exit —
        // the `while` (not `if`) re-checks the flag after waking. Regressing this
        // to `if` would let the background reader race elicit()/confirm() again.
        coord.resume.notify_one();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !waiter.is_finished(),
            "waiter must not exit while elicit_active remains true"
        );

        coord.elicit_active.store(false, Ordering::Release);
        coord.resume.notify_one();

        tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("wait loop should exit within timeout")
            .expect("waiter task should not panic");
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
            stdin_coord: Arc::new(StdinCoordination::new()),
        };

        // Pre-fill the channel with a message (simulates background reader
        // having already buffered input before select! cancellation).
        tx.send(ChannelMessage {
            text: "hello".to_string(),
            attachments: vec![],
            is_guest_context: false,
            is_from_bot: false,
            owner_key: None,
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
        assert_matches!(result, Ok(None));
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
        assert_matches!(result, Ok(None));
        assert!(pending.is_empty());
    }

    // `process_line`'s rejection messages go to stdout via `println!` rather than a
    // return value, so `image_path_rejection_message` (the classifier->message wiring)
    // is the directly-testable seam.

    #[test]
    fn image_path_rejection_message_absolute() {
        let msg = image_path_rejection_message(PathRejection::Absolute).unwrap();
        assert!(msg.contains("absolute paths are not supported"));
    }

    #[test]
    fn image_path_rejection_message_traversal() {
        let msg = image_path_rejection_message(PathRejection::Traversal).unwrap();
        assert!(msg.contains("path traversal") && msg.contains("not allowed"));
    }

    #[test]
    fn image_path_rejection_message_allowed_is_none() {
        assert!(image_path_rejection_message(PathRejection::Allowed).is_none());
    }
}
