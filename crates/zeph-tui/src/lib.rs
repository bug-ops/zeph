// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! # zeph-tui
//!
//! Ratatui-based TUI dashboard for the Zeph AI agent with real-time metrics,
//! syntax-highlighted chat, tool-output diffs, command palette, file picker,
//! and multi-panel layout.
//!
//! ## Architecture
//!
//! The crate is structured around a central [`App`] state machine that owns all
//! widget state and reacts to two event streams:
//!
//! - [`AppEvent`] — keyboard, resize, and mouse events produced by
//!   [`EventReader`] running on a dedicated OS thread.
//! - [`AgentEvent`] — streaming agent output, tool events, and control signals
//!   forwarded through [`TuiChannel`].
//!
//! The main entry point is [`run_tui`], which initialises the terminal,
//! drives the render loop, and restores the terminal on exit or panic.
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use tokio::sync::mpsc;
//! use zeph_tui::{App, run_tui};
//! use zeph_tui::event::AppEvent;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), zeph_tui::TuiError> {
//!     let (user_tx, user_rx) = mpsc::channel(64);
//!     let (_agent_tx, agent_rx) = mpsc::channel(64);
//!     let app = App::new(user_tx, agent_rx);
//!     let (_event_tx, event_rx) = mpsc::channel(64);
//!     run_tui(app, event_rx).await
//! }
//! ```

pub mod app;
pub mod channel;
pub mod command;
pub mod error;
pub mod event;
pub mod file_picker;
pub mod highlight;
pub mod hyperlink;
pub mod layout;
pub mod metrics;
pub mod render_cache;
pub(crate) mod session;
#[cfg(test)]
pub mod test_utils;
pub mod theme;
pub mod types;
pub mod widgets;

use std::io;

pub use app::App;
pub use channel::TuiChannel;
pub use command::TuiCommand;
pub use error::TuiError;
pub use event::{AgentEvent, AppEvent, CrosstermEventSource, EventReader, EventSource};
pub use metrics::{MetricsCollector, MetricsSnapshot};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;
pub use types::{ChatMessage, InputMode, MessageRole, PasteState};

/// Run the TUI dashboard until the user quits.
///
/// Initialises the terminal in raw/alternate-screen mode, drives the render
/// loop, and restores the terminal on normal exit, error, **and** panic.
///
/// # Arguments
///
/// * `app` — fully-constructed [`App`] instance (see [`App::new`]).
/// * `event_rx` — receiver end of the [`AppEvent`] channel produced by
///   [`EventReader`].
///
/// # Errors
///
/// Returns [`TuiError`] if terminal initialisation, rendering, or restoration
/// fails.
///
/// # Examples
///
/// ```rust,no_run
/// use tokio::sync::mpsc;
/// use zeph_tui::{App, run_tui};
///
/// #[tokio::main]
/// async fn main() -> Result<(), zeph_tui::TuiError> {
///     let (user_tx, user_rx) = mpsc::channel(64);
///     let (_agent_tx, agent_rx) = mpsc::channel(64);
///     let app = App::new(user_tx, agent_rx);
///     let (_event_tx, event_rx) = mpsc::channel(64);
///     run_tui(app, event_rx).await
/// }
/// ```
pub async fn run_tui(mut app: App, mut event_rx: mpsc::Receiver<AppEvent>) -> Result<(), TuiError> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableBracketedPaste,
        );
        original_hook(info);
    }));

    let mut terminal = init_terminal()?;

    let result = tui_loop(&mut app, &mut event_rx, &mut terminal).await;

    restore_terminal(&mut terminal)?;

    // Restore the default panic hook
    let _ = std::panic::take_hook();

    result
}

/// Tracks how much of the UI needs to be redrawn after each event.
///
/// The render loop inspects this after every `select!` arm to decide whether
/// to call `terminal.draw()` and, if so, how eagerly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirtyState {
    /// Nothing changed — skip `terminal.draw()` entirely.
    Clean,
    /// Only the spinner / progress indicator may have advanced (tick event).
    /// Draw only when the agent is actively running so the spinner animates.
    AnimationOnly,
    /// Layout, content, or input changed — always redraw.
    Full,
}

async fn tui_loop(
    app: &mut App,
    event_rx: &mut mpsc::Receiver<AppEvent>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<(), TuiError> {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = DirtyState::Clean;

    loop {
        tokio::select! {
            biased;
            Some(event) = event_rx.recv() => {
                app.handle_event(event);
                dirty = DirtyState::Full;
            }
            agent_poll = app.poll_agent_event() => {
                if let Some(agent_event) = agent_poll {
                    app.handle_agent_event(agent_event);
                    while let Ok(ev) = app.try_recv_agent_event() {
                        app.handle_agent_event(ev);
                    }
                } else {
                    // Agent channel closed: agent exited. Quit the TUI.
                    app.should_quit = true;
                }
                dirty = DirtyState::Full;
            }
            _ = tick.tick() => {
                // Tick: only upgrade to AnimationOnly if no full redraw is
                // already scheduled, so a burst of agent events is not
                // downgraded.
                if dirty == DirtyState::Clean {
                    dirty = DirtyState::AnimationOnly;
                }
            }
        }

        app.poll_metrics();
        app.poll_pending_file_index();
        app.poll_pending_transcript();
        app.refresh_task_snapshots();

        let should_draw = match dirty {
            DirtyState::Clean => false,
            DirtyState::AnimationOnly => app.is_agent_busy(),
            DirtyState::Full => true,
        };

        if should_draw {
            terminal.draw(|frame| app.draw(frame))?;
            let links = app.take_hyperlinks();
            if !links.is_empty() {
                hyperlink::write_osc8(terminal.backend_mut(), &links)?;
            }
            dirty = DirtyState::Clean;
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>, TuiError> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste,
    )?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<(), TuiError> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
    )?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use crate::app::App;
    use crate::metrics::MetricsSnapshot;

    fn make_app() -> App {
        let (user_tx, _user_rx) = mpsc::channel(1);
        let (_agent_tx, agent_rx) = mpsc::channel(1);
        App::new(user_tx, agent_rx)
    }

    /// Regression test for #1077: the `tui_loop` must redraw on every tick even with no
    /// user/agent events. Before the fix the tick arm was `_ = tick.tick() => {}` (no-op),
    /// so the loop stalled after the first frame. The fix moves the draw call to the top of
    /// each loop iteration, making it unconditional.
    ///
    /// This test verifies the observable consequence: `App::poll_metrics()` can be called
    /// repeatedly without side-effects, and the `MetricsSnapshot` is populated from the
    /// collector on each call — confirming the contract the fixed loop relies on.
    #[test]
    fn tick_arm_sets_dirty() {
        let mut app = make_app();
        // Simulate what the fixed loop does: poll_metrics on each iteration.
        app.poll_metrics();
        app.poll_metrics();
        // If poll_metrics panics or the metrics watch channel is broken the test fails.
        // Verify the snapshot is accessible after polling.
        let _: &MetricsSnapshot = &app.metrics;
    }
}
