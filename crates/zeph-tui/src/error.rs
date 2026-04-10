// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Errors produced by the TUI subsystem.
///
/// All variants implement [`std::error::Error`] via [`thiserror`] and carry a
/// source error for full cause chains.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::TuiError;
///
/// fn check(e: &TuiError) -> bool {
///     matches!(e, TuiError::Io(_))
/// }
///
/// let io_err = std::io::Error::other("disk full");
/// let tui_err = TuiError::from(io_err);
/// assert!(check(&tui_err));
/// ```
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// A terminal I/O operation failed (e.g. enabling raw mode or drawing).
    #[error("terminal I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The agent channel reported an error (e.g. the channel was closed unexpectedly).
    #[error("channel error: {0}")]
    Channel(#[from] zeph_core::channel::ChannelError),
}
