// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Clipboard integration for the TUI dashboard.
//!
//! Provides [`ClipboardHandle`] which writes text to the system clipboard via
//! `arboard` when available, with an OSC 52 escape-sequence fallback for SSH
//! sessions and environments where `arboard` is unavailable or fails.

use crate::error::TuiError;

/// Detect whether the process is running inside an SSH session.
///
/// Checks all three standard SSH environment variables: `SSH_TTY`,
/// `SSH_CONNECTION`, and `SSH_CLIENT`. Some minimal SSH implementations (e.g.,
/// dropbear) only set `SSH_CLIENT`, so all three must be checked.
///
/// Uses [`std::env::var_os`] to avoid `Err(NotUnicode)` on non-UTF-8 values.
#[cfg(feature = "clipboard")]
fn is_ssh() -> bool {
    std::env::var_os("SSH_TTY").is_some()
        || std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_CLIENT").is_some()
}

/// Write `text` to the terminal clipboard via the OSC 52 escape sequence.
///
/// The sequence is written to stderr to avoid interfering with ratatui's
/// ownership of stdout in raw/alternate-screen mode. Flushes after writing
/// to guarantee delivery before the next ratatui frame.
#[cfg(feature = "clipboard")]
fn write_osc52(text: &str) -> Result<(), TuiError> {
    use base64::Engine as _;
    use std::io::Write as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{encoded}\x07");
    let mut stderr = std::io::stderr();
    stderr.write_all(seq.as_bytes()).map_err(TuiError::from)?;
    stderr.flush().map_err(TuiError::from)?;
    Ok(())
}

/// A handle to the system clipboard.
///
/// On local sessions, attempts to write via `arboard` and falls back to OSC 52
/// on failure. On SSH sessions (detected via `SSH_TTY`, `SSH_CONNECTION`, or
/// `SSH_CLIENT` at construction time), OSC 52 is used directly.
///
/// SSH status is detected once in [`new`](Self::new) and cached as a field;
/// the detection is not repeated on each [`copy`](Self::copy) call.
///
/// When the `clipboard` feature is disabled, [`copy`](Self::copy) is a no-op.
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_tui::clipboard::ClipboardHandle;
///
/// let mut handle = ClipboardHandle::new();
/// handle.copy("hello").unwrap();
/// ```
pub struct ClipboardHandle {
    #[cfg(feature = "clipboard")]
    inner: Option<arboard::Clipboard>,
    /// `true` when the process was launched inside an SSH session.
    ///
    /// Computed once at construction; SSH status cannot change during a session.
    #[cfg(feature = "clipboard")]
    is_ssh: bool,
}

impl ClipboardHandle {
    /// Create a new clipboard handle.
    ///
    /// Detects SSH status once and caches it. On the `clipboard` feature path,
    /// attempts to open `arboard::Clipboard` eagerly; stores `None` if
    /// initialisation fails (graceful degradation to OSC 52).
    #[must_use]
    pub fn new() -> Self {
        #[cfg(feature = "clipboard")]
        {
            let is_ssh = is_ssh();
            let inner = arboard::Clipboard::new().ok();
            Self { inner, is_ssh }
        }
        #[cfg(not(feature = "clipboard"))]
        Self {}
    }

    /// Copy `text` to the clipboard.
    ///
    /// Uses OSC 52 when running over SSH. Otherwise tries the native clipboard
    /// via `arboard` and falls back to OSC 52 on failure.
    ///
    /// When the `clipboard` feature is disabled, this is a no-op returning `Ok`.
    ///
    /// # Errors
    ///
    /// Returns [`TuiError::Io`] if writing the OSC 52 escape sequence to stderr
    /// fails.
    pub fn copy(&mut self, text: &str) -> Result<(), TuiError> {
        #[cfg(feature = "clipboard")]
        {
            if self.is_ssh {
                return write_osc52(text);
            }
            if let Some(ref mut cb) = self.inner
                && cb.set_text(text).is_ok()
            {
                return Ok(());
            }
            // arboard unavailable or failed — fall back to OSC 52
            write_osc52(text)
        }
        #[cfg(not(feature = "clipboard"))]
        {
            let _ = text;
            Ok(())
        }
    }
}

impl Default for ClipboardHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_handle_default_does_not_panic() {
        let _ = ClipboardHandle::default();
    }

    #[test]
    fn clipboard_handle_new_does_not_panic() {
        let _ = ClipboardHandle::new();
    }

    #[cfg(feature = "clipboard")]
    #[test]
    fn is_ssh_false_when_env_absent() {
        // Ensure none of the SSH env vars are set (they aren't in a cargo test environment).
        // If running inside SSH, this test will correctly reflect that — skip the assertion.
        if std::env::var_os("SSH_TTY").is_none()
            && std::env::var_os("SSH_CONNECTION").is_none()
            && std::env::var_os("SSH_CLIENT").is_none()
        {
            assert!(!is_ssh());
        }
    }

    #[cfg(feature = "clipboard")]
    #[test]
    fn osc52_base64_encodes_payload() {
        use base64::Engine as _;
        let text = "hello clipboard";
        let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        // Verify base64 output contains only safe characters (no control chars).
        assert!(
            encoded
                .chars()
                .all(|c| c.is_alphanumeric() || c == '+' || c == '/' || c == '=')
        );
    }
}
