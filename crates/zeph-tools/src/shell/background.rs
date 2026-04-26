// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Background shell execution registry and associated types.
//!
//! This module provides the [`RunId`] newtype for tracking individual background
//! shell runs, and the [`BackgroundHandle`] struct used by `ShellExecutor` to
//! manage in-flight processes.
//!
//! Background runs are stored in a `HashMap<RunId, BackgroundHandle>` on the
//! executor. The registry is bounded by `max_background_runs` from config.

use std::time::Instant;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Opaque correlation identifier for a background shell run.
///
/// The inner field is private: external code cannot construct a `RunId` that
/// collides with an existing registry entry. Displays as a 32-character
/// lowercase hex string so the LLM can reference it in follow-up turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct RunId(Uuid);

impl RunId {
    /// Generate a new random `RunId`.
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0.as_u128())
    }
}

/// Registry entry for an in-flight background shell run.
#[derive(Debug)]
pub(crate) struct BackgroundHandle {
    /// Command string, stored for shutdown reporting and TUI display.
    pub command: String,
    /// Wall-clock start time for elapsed reporting.
    // TODO(#3448): expose via TUI panel for per-run elapsed display.
    #[allow(dead_code)]
    pub started_at: Instant,
    /// Cancellation token. Cancel to request graceful shutdown.
    pub abort: CancellationToken,
    /// OS process ID, if known. Reserved for future SIGTERM escalation on shutdown.
    // TODO(#3449): use once safe signal-sending wrapper (e.g. nix crate) is available.
    #[allow(dead_code)]
    pub child_pid: Option<u32>,
}

/// Final result delivered when a background run finishes.
///
/// Sent via `ToolEvent::Completed { run_id: Some(..), .. }` and buffered in
/// `LifecycleState::pending_background_completions` for injection into the next turn.
#[derive(Debug, Clone)]
pub struct BackgroundCompletion {
    /// The run that produced this result.
    pub run_id: RunId,
    /// Shell exit code (`0` = success).
    pub exit_code: i32,
    /// Filtered and truncated output text.
    pub output: String,
    /// `true` when `exit_code == 0`.
    pub success: bool,
    /// Wall-clock elapsed milliseconds from spawn to completion.
    pub elapsed_ms: u64,
    /// Original command string.
    pub command: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn run_id_display_is_32_char_hex() {
        let id = RunId::new();
        let s = id.to_string();
        assert_eq!(s.len(), 32, "RunId should display as 32-char hex");
        assert!(
            s.chars().all(|c| c.is_ascii_hexdigit()),
            "RunId should be lowercase hex, got: {s}"
        );
    }

    #[test]
    fn run_id_uniqueness() {
        let ids: HashSet<String> = (0..100).map(|_| RunId::new().to_string()).collect();
        assert_eq!(ids.len(), 100, "100 RunIds must all be distinct");
    }

    #[test]
    fn run_id_copy_semantics() {
        let a = RunId::new();
        let b = a; // Copy, not move
        assert_eq!(a, b);
    }
}
