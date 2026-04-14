// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Runtime mode flags determined at startup from CLI arguments.
///
/// This struct is intentionally `Copy` — it carries only boolean flags
/// and is passed by value to subsystem initializers. Adding it to function
/// signatures replaces individual `tui_mode: bool` parameters and provides
/// a single extension point for future runtime flags.
///
/// # Examples
///
/// ```
/// use zeph_core::RuntimeContext;
///
/// let ctx = RuntimeContext { tui_mode: true, daemon_mode: false };
/// assert!(ctx.suppress_stderr());
///
/// let default_ctx = RuntimeContext::default();
/// assert!(!default_ctx.suppress_stderr());
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeContext {
    /// True when the TUI dashboard is active (ratatui owns stderr).
    pub tui_mode: bool,
    /// True when running as a headless daemon (a2a feature).
    ///
    /// This field is forward-looking: it is set at daemon entry but has no
    /// current consumer beyond [`RuntimeContext::suppress_stderr`]. When the
    /// a2a subsystem grows additional mode-aware initializers, they will read
    /// this field rather than threading a new `daemon_mode: bool` parameter.
    pub daemon_mode: bool,
}

impl RuntimeContext {
    /// Returns `true` when stderr output should be suppressed.
    ///
    /// Stderr is suppressed when the TUI owns the terminal (raw mode) or when
    /// running as a headless daemon with no controlling terminal.
    #[must_use]
    pub fn suppress_stderr(&self) -> bool {
        self.tui_mode || self.daemon_mode
    }
}

#[cfg(test)]
mod tests {
    use super::RuntimeContext;

    #[test]
    fn suppress_stderr_daemon_only() {
        let ctx = RuntimeContext {
            tui_mode: false,
            daemon_mode: true,
        };
        assert!(ctx.suppress_stderr());
    }

    #[test]
    fn suppress_stderr_both_true() {
        let ctx = RuntimeContext {
            tui_mode: true,
            daemon_mode: true,
        };
        assert!(ctx.suppress_stderr());
    }

    #[test]
    fn suppress_stderr_default_is_false() {
        assert!(!RuntimeContext::default().suppress_stderr());
    }
}
