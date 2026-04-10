// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, MouseEventKind};
use tokio::sync::{Notify, mpsc, oneshot, watch};

use zeph_core::metrics::MetricsSnapshot;

/// Source of raw terminal events consumed by [`EventReader`].
///
/// Implement this trait to provide a custom event source (e.g. a mock for
/// testing or a replay driver).
///
/// # Examples
///
/// ```rust
/// use zeph_tui::event::{AppEvent, EventSource};
/// use crossterm::event::KeyEvent;
///
/// struct OneTickSource;
///
/// impl EventSource for OneTickSource {
///     fn next_event(&mut self) -> Option<AppEvent> {
///         None // signal EOF
///     }
/// }
/// ```
pub trait EventSource: Send + 'static {
    /// Return the next event, or `None` to signal that the source is exhausted
    /// and the event loop should terminate.
    fn next_event(&mut self) -> Option<AppEvent>;
}

/// [`EventSource`] backed by crossterm's blocking event poll.
///
/// Polls for terminal events up to `tick_rate` before returning a
/// [`AppEvent::Tick`] if no event arrived. This drives the TUI's animation
/// and idle redraw cadence.
///
/// # Examples
///
/// ```rust
/// use std::time::Duration;
/// use zeph_tui::CrosstermEventSource;
///
/// let source = CrosstermEventSource::new(Duration::from_millis(250));
/// ```
pub struct CrosstermEventSource {
    tick_rate: Duration,
}

impl CrosstermEventSource {
    /// Create a new source with the given poll interval.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use zeph_tui::CrosstermEventSource;
    ///
    /// let src = CrosstermEventSource::new(Duration::from_millis(100));
    /// ```
    #[must_use]
    pub fn new(tick_rate: Duration) -> Self {
        Self { tick_rate }
    }
}

impl EventSource for CrosstermEventSource {
    fn next_event(&mut self) -> Option<AppEvent> {
        if event::poll(self.tick_rate).unwrap_or(false) {
            match event::read() {
                Ok(CrosstermEvent::Key(key)) => Some(AppEvent::Key(key)),
                Ok(CrosstermEvent::Resize(w, h)) => Some(AppEvent::Resize(w, h)),
                Ok(CrosstermEvent::Mouse(mouse)) => match mouse.kind {
                    MouseEventKind::ScrollUp => Some(AppEvent::MouseScroll(1)),
                    MouseEventKind::ScrollDown => Some(AppEvent::MouseScroll(-1)),
                    _ => Some(AppEvent::Tick),
                },
                _ => Some(AppEvent::Tick),
            }
        } else {
            Some(AppEvent::Tick)
        }
    }
}

/// Top-level event consumed by the [`crate::App`] event handler.
///
/// Events arrive from two sources:
/// - Terminal input via [`EventReader`] / [`CrosstermEventSource`].
/// - Agent output forwarded through [`AgentEvent`] by [`crate::TuiChannel`].
///
/// # Examples
///
/// ```rust
/// use zeph_tui::event::AppEvent;
///
/// let ev = AppEvent::Tick;
/// assert!(matches!(ev, AppEvent::Tick));
/// ```
#[derive(Debug)]
pub enum AppEvent {
    /// A keyboard event from crossterm.
    Key(KeyEvent),
    /// Periodic tick used to drive animations and idle redraws.
    Tick,
    /// The terminal was resized to the given `(columns, rows)`.
    Resize(u16, u16),
    /// Mouse wheel scroll: `+1` = up, `-1` = down.
    MouseScroll(i8),
    /// An event forwarded from the agent event channel.
    Agent(AgentEvent),
}

/// Events produced by the agent and forwarded to the TUI via [`crate::TuiChannel`].
///
/// Each variant corresponds to a distinct phase or signal in the agent lifecycle
/// (streaming output, tool execution, user confirmation, etc.).
///
/// # Examples
///
/// ```rust
/// use zeph_tui::event::AgentEvent;
///
/// let ev = AgentEvent::Chunk("partial response".to_string());
/// assert!(matches!(ev, AgentEvent::Chunk(_)));
/// ```
#[derive(Debug)]
pub enum AgentEvent {
    /// A streaming text chunk from the LLM — appended to the current message.
    Chunk(String),
    /// A complete (non-streaming) assistant message.
    FullMessage(String),
    /// Signals that streaming is complete; the chat widget stops the cursor.
    Flush,
    /// The agent is waiting for an LLM response (drives the throbber).
    Typing,
    /// A short status string to display in the activity bar (e.g. `"Searching memory…"`).
    Status(String),
    /// A tool call has started; the TUI should display a spinner with the tool name.
    ToolStart {
        /// Canonical tool name (e.g. `"bash"`, `"read_file"`).
        tool_name: String,
        /// The primary command or argument string shown in the status bar.
        command: String,
    },
    /// An incremental output chunk from a long-running tool (e.g. streaming shell output).
    ToolOutputChunk {
        /// Tool that produced the chunk.
        tool_name: String,
        /// Command argument associated with the tool call.
        command: String,
        /// The chunk text to append.
        chunk: String,
    },
    /// Final tool output, replacing any in-progress chunks for this call.
    ToolOutput {
        /// Tool that produced the output.
        tool_name: String,
        /// Command argument associated with the tool call.
        command: String,
        /// Full rendered output body.
        output: String,
        /// `true` if the tool succeeded, `false` on error.
        success: bool,
        /// Optional diff to display inline in the chat.
        diff: Option<zeph_core::DiffData>,
        /// Human-readable filter summary, if output was filtered.
        filter_stats: Option<String>,
        /// Indices of lines retained by the filter.
        kept_lines: Option<Vec<usize>>,
    },
    /// The agent requests a boolean confirmation from the user.
    ConfirmRequest {
        /// Prompt text shown in the confirmation dialog.
        prompt: String,
        /// One-shot channel to send the user's `true`/`false` response.
        response_tx: oneshot::Sender<bool>,
    },
    /// The agent requests structured input via an elicitation dialog.
    ElicitationRequest {
        /// The elicitation schema and prompt.
        request: zeph_core::channel::ElicitationRequest,
        /// One-shot channel to send the user's response.
        response_tx: oneshot::Sender<zeph_core::channel::ElicitationResponse>,
    },
    /// Updated count of messages queued for the agent (shown in the input bar).
    QueueCount(usize),
    /// A diff is ready for immediate display in the diff panel.
    DiffReady(zeph_core::DiffData),
    /// Result from a slash-command dispatched to the agent.
    CommandResult {
        /// The slash-command identifier that produced this result.
        command_id: String,
        /// Formatted command output to display.
        output: String,
    },
    /// Wire a cancel signal into the TUI App after early startup (Phase 2).
    SetCancelSignal(Arc<Notify>),
    /// Wire a metrics receiver into the TUI App after early startup (Phase 2).
    SetMetricsRx(watch::Receiver<MetricsSnapshot>),
}

/// Blocking event pump that forwards terminal events to the async [`AppEvent`] channel.
///
/// `EventReader` must run on a **dedicated `std::thread`** — it calls
/// `blocking_send` and crossterm's blocking poll, which would stall a tokio
/// worker thread.
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::Duration;
/// use tokio::sync::mpsc;
/// use zeph_tui::EventReader;
///
/// let (tx, rx) = mpsc::channel(64);
/// let reader = EventReader::new(tx, Duration::from_millis(250));
/// std::thread::spawn(|| reader.run());
/// ```
pub struct EventReader {
    tx: mpsc::Sender<AppEvent>,
    tick_rate: Duration,
}

impl EventReader {
    /// Create a new reader that sends events to `tx` at up to `tick_rate` cadence.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use tokio::sync::mpsc;
    /// use zeph_tui::EventReader;
    ///
    /// let (tx, _rx) = mpsc::channel(64);
    /// let reader = EventReader::new(tx, Duration::from_millis(250));
    /// ```
    #[must_use]
    pub fn new(tx: mpsc::Sender<AppEvent>, tick_rate: Duration) -> Self {
        Self { tx, tick_rate }
    }

    /// Start the blocking event loop using the default [`CrosstermEventSource`].
    ///
    /// **Must be called from a dedicated `std::thread`**, not a tokio worker.
    /// Returns when the [`AppEvent`] channel receiver is dropped.
    pub fn run(self) {
        let tick_rate = self.tick_rate;
        self.run_with_source(CrosstermEventSource::new(tick_rate));
    }

    /// Start the blocking event loop with a custom [`EventSource`].
    ///
    /// This variant exists primarily for testing with mock sources.
    /// Returns when the source returns `None` or the channel is closed.
    pub fn run_with_source(self, mut source: impl EventSource) {
        while let Some(evt) = source.next_event() {
            if self.tx.blocking_send(evt).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_debug() {
        let e = AgentEvent::Chunk("hello".into());
        let s = format!("{e:?}");
        assert!(s.contains("Chunk"));
    }

    #[test]
    fn app_event_variants() {
        let tick = AppEvent::Tick;
        assert!(matches!(tick, AppEvent::Tick));

        let resize = AppEvent::Resize(80, 24);
        assert!(matches!(resize, AppEvent::Resize(80, 24)));
    }

    #[test]
    fn event_reader_construction() {
        let (tx, _rx) = mpsc::channel(16);
        let reader = EventReader::new(tx, Duration::from_millis(100));
        assert_eq!(reader.tick_rate, Duration::from_millis(100));
    }

    #[test]
    fn confirm_request_debug() {
        let (tx, _rx) = oneshot::channel();
        let e = AgentEvent::ConfirmRequest {
            prompt: "delete?".into(),
            response_tx: tx,
        };
        let s = format!("{e:?}");
        assert!(s.contains("ConfirmRequest"));
        assert!(s.contains("delete?"));
    }

    #[test]
    fn app_event_mouse_scroll_variant() {
        let scroll_up = AppEvent::MouseScroll(1);
        assert!(matches!(scroll_up, AppEvent::MouseScroll(1)));

        let scroll_down = AppEvent::MouseScroll(-1);
        assert!(matches!(scroll_down, AppEvent::MouseScroll(-1)));
    }
}
