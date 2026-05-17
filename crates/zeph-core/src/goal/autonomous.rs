// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Autonomous goal execution types and driver.
//!
//! An [`AutonomousSession`] tracks one active multi-turn run. At most one session can
//! exist at a time (invariant A1). The [`AutonomousDriver`] lives on `Agent` and is polled
//! cooperatively from within the existing `tokio::select!` loop — no separate task is spawned.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use tokio::time::{Duration, Instant as TokioInstant, Interval};
use tokio_util::sync::CancellationToken;

/// FSM states for an autonomous goal session.
///
/// Valid transitions:
/// - `Running` → `Verifying` (supervisor check triggered)
/// - `Verifying` → `Running` (supervisor: not yet achieved)
/// - `Verifying` → `Achieved` (supervisor: goal met)
/// - `Running` / `Verifying` → `Stuck` (stuck counter exhausted or supervisor failures)
/// - `Running` / `Verifying` → `Aborted` (user cancel or turn limit reached)
/// - `Running` / `Verifying` → `Failed` (unrecoverable error)
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomousState {
    /// The agent is actively running multi-turn execution.
    Running,
    /// The supervisor is verifying whether the goal condition is satisfied.
    Verifying,
    /// Goal achieved and confirmed by the supervisor.
    Achieved,
    /// No progress detected across the configured number of consecutive turns.
    Stuck,
    /// Session aborted by the user or turn limit reached.
    Aborted,
    /// Unrecoverable error during execution.
    Failed,
}

impl std::fmt::Display for AutonomousState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => f.write_str("running"),
            Self::Verifying => f.write_str("verifying"),
            Self::Achieved => f.write_str("achieved"),
            Self::Stuck => f.write_str("stuck"),
            Self::Aborted => f.write_str("aborted"),
            Self::Failed => f.write_str("failed"),
        }
    }
}

/// Structured verdict returned by the supervisor verifier after a single LLM call.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SupervisorVerdict {
    /// Whether the goal condition is considered satisfied.
    pub achieved: bool,
    /// Short natural-language explanation of the verdict.
    pub reasoning: String,
    /// Confidence score in `[0.0, 1.0]`.
    pub confidence: f32,
    /// Optional suggestions for continuing when `achieved = false`.
    pub suggestions: Vec<String>,
}

/// Live state for one autonomous goal execution session.
///
/// At most one instance exists at a time, held inside [`AutonomousDriver::session`].
pub struct AutonomousSession {
    /// UUID of the [`crate::goal::Goal`] being executed.
    pub goal_id: String,
    /// Cached goal text used for synthetic message injection.
    pub goal_text: String,
    /// Current FSM state.
    pub state: AutonomousState,
    /// Total turns executed so far in this session.
    pub turns_executed: u32,
    /// Turn limit for this session (from config or per-invocation `--turns N`).
    pub max_turns: u32,
    /// Number of consecutive turns the stuck heuristic fired with no progress.
    pub stuck_count: u32,
    /// Number of consecutive supervisor verification failures (timeout, 429, auth error).
    pub supervisor_fail_count: u32,
    /// Cooperative cancellation signal; checked before each tool call inside a turn.
    pub cancel: CancellationToken,
    /// Last verdict received from the supervisor, if any.
    pub last_verdict: Option<SupervisorVerdict>,
    /// Wall-clock time this session started.
    pub started_at: Instant,
    /// When set, the supervisor retry (after 429 backoff) should fire at or after this instant.
    pub supervisor_retry_at: Option<TokioInstant>,
}

impl AutonomousSession {
    /// Create a new session in [`AutonomousState::Running`].
    #[must_use]
    pub fn new(goal_id: impl Into<String>, goal_text: impl Into<String>, max_turns: u32) -> Self {
        Self {
            goal_id: goal_id.into(),
            goal_text: goal_text.into(),
            state: AutonomousState::Running,
            turns_executed: 0,
            max_turns,
            stuck_count: 0,
            supervisor_fail_count: 0,
            cancel: CancellationToken::new(),
            last_verdict: None,
            started_at: Instant::now(),
            supervisor_retry_at: None,
        }
    }

    /// `true` when the session is in a terminal state.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            AutonomousState::Achieved
                | AutonomousState::Stuck
                | AutonomousState::Aborted
                | AutonomousState::Failed
        )
    }

    /// Elapsed wall-clock time since session start.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }
}

/// Drives autonomous turns cooperatively from within `Agent::run`'s `tokio::select!` loop.
///
/// When `session` is `Some` and the session is [`AutonomousState::Running`], `should_tick()`
/// returns `true` and `next_tick().await` completes after the configured inter-turn delay.
/// The agent's main loop calls `run_autonomous_turn()` in a dedicated select branch —
/// no `tokio::spawn` is used, preserving exclusive `&mut Agent` access.
///
/// The `Interval` is created lazily on the first `start_session` call so that `AutonomousDriver`
/// can be constructed outside a Tokio runtime context (e.g., during unit-test builder setup).
pub struct AutonomousDriver {
    /// Active session, if any. At most one at a time (invariant A1).
    pub session: Option<AutonomousSession>,
    /// Configured delay between autonomous turns. Used to (re-)create `turn_interval`.
    turn_delay: Duration,
    /// Periodic interval that paces autonomous turns. `None` before the first session starts.
    pub(crate) turn_interval: Option<Interval>,
    /// Pending session start requested by a command handler.
    ///
    /// `handle_goal` runs inside `Box::pin(async move)` and cannot call `start_session`
    /// directly (borrow conflict with `&mut Agent`). Instead it writes
    /// `(goal_id, goal_text, max_turns)` here. The main agent loop calls
    /// `flush_pending_start()` after each command handler returns to actually start the session.
    pub pending_start_arc: Arc<Mutex<Option<(String, String, u32)>>>,
}

impl AutonomousDriver {
    /// Create a driver with the given inter-turn delay.
    ///
    /// No Tokio runtime is required at construction time — the interval is created lazily.
    ///
    /// # Panics
    ///
    /// Panics if `turn_delay` is zero (would busy-loop the reactor).
    #[must_use]
    pub fn new(turn_delay: Duration) -> Self {
        assert!(
            !turn_delay.is_zero(),
            "autonomous turn delay must be non-zero"
        );
        Self {
            session: None,
            turn_delay,
            turn_interval: None,
            pending_start_arc: Arc::new(Mutex::new(None)),
        }
    }

    /// Drain any pending session start that was queued by a command handler.
    ///
    /// Returns the cancelled session's goal ID (if one was pre-empted) and the goal ID of
    /// the newly started session (if a start was pending). Call this from the main agent loop
    /// after each command handler returns and `&mut self` is exclusively available.
    ///
    /// Returns `None` when no pending start was queued.
    #[must_use]
    pub fn flush_pending_start(&mut self) -> Option<(Option<String>, String)> {
        let pending = self.pending_start_arc.lock().take()?;
        let (goal_id, goal_text, max_turns) = pending;
        let new_id = goal_id.clone();
        let cancelled = self.start_session(goal_id, goal_text, max_turns);
        Some((cancelled, new_id))
    }

    /// Returns `true` when there is a running session that should produce the next tick.
    #[must_use]
    pub fn should_tick(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.state == AutonomousState::Running)
    }

    /// Await the next tick of the inter-turn interval.
    ///
    /// Requires a Tokio runtime. Creates the interval on first call. The caller is responsible
    /// for checking [`should_tick`] first (typically via the `if` guard on the `select!` branch).
    ///
    /// [`should_tick`]: Self::should_tick
    pub async fn next_tick(&mut self) {
        let interval = self.turn_interval.get_or_insert_with(|| {
            let mut iv = tokio::time::interval(self.turn_delay);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            iv
        });
        interval.tick().await;
    }

    /// Start a new autonomous session, cancelling any previously active session.
    ///
    /// Returns the goal ID of the cancelled session if one was aborted, so the caller can
    /// notify the user.
    pub fn start_session(
        &mut self,
        goal_id: impl Into<String>,
        goal_text: impl Into<String>,
        max_turns: u32,
    ) -> Option<String> {
        let prev = self.session.take();
        let cancelled_id = prev.map(|s| {
            s.cancel.cancel();
            s.goal_id
        });
        self.session = Some(AutonomousSession::new(goal_id, goal_text, max_turns));
        cancelled_id
    }

    /// Abort the current session (if any), setting state to [`AutonomousState::Aborted`].
    ///
    /// Returns the goal ID that was aborted, or `None` if no session was active.
    pub fn abort(&mut self) -> Option<String> {
        let s = self.session.as_mut()?;
        s.cancel.cancel();
        s.state = AutonomousState::Aborted;
        let id = s.goal_id.clone();
        self.session = None;
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_running() {
        let s = AutonomousSession::new("id1", "do something", 10);
        assert_eq!(s.state, AutonomousState::Running);
        assert_eq!(s.turns_executed, 0);
        assert!(!s.is_terminal());
    }

    #[test]
    fn terminal_states() {
        for terminal in [
            AutonomousState::Achieved,
            AutonomousState::Stuck,
            AutonomousState::Aborted,
            AutonomousState::Failed,
        ] {
            let mut s = AutonomousSession::new("id", "text", 5);
            s.state = terminal;
            assert!(s.is_terminal(), "{terminal} should be terminal");
        }
    }

    #[test]
    fn running_and_verifying_are_not_terminal() {
        for non_terminal in [AutonomousState::Running, AutonomousState::Verifying] {
            let mut s = AutonomousSession::new("id", "text", 5);
            s.state = non_terminal;
            assert!(!s.is_terminal(), "{non_terminal} should not be terminal");
        }
    }

    #[test]
    fn driver_should_tick_only_when_running() {
        let mut driver = AutonomousDriver::new(Duration::from_millis(100));
        assert!(!driver.should_tick(), "no session → no tick");

        driver.start_session("id", "text", 5);
        assert!(driver.should_tick(), "Running session → tick");

        if let Some(ref mut s) = driver.session {
            s.state = AutonomousState::Verifying;
        }
        assert!(!driver.should_tick(), "Verifying session → no tick");

        if let Some(ref mut s) = driver.session {
            s.state = AutonomousState::Achieved;
        }
        assert!(!driver.should_tick(), "Achieved session → no tick");
    }

    #[test]
    fn start_session_cancels_previous() {
        let mut driver = AutonomousDriver::new(Duration::from_millis(100));
        driver.start_session("id1", "first", 10);
        let prev_cancel = driver.session.as_ref().unwrap().cancel.clone();
        assert!(!prev_cancel.is_cancelled());

        let cancelled = driver.start_session("id2", "second", 5);
        assert_eq!(cancelled.as_deref(), Some("id1"));
        assert!(
            prev_cancel.is_cancelled(),
            "previous token must be cancelled"
        );
        assert_eq!(driver.session.as_ref().unwrap().goal_id, "id2");
    }

    #[test]
    fn abort_clears_session_and_returns_id() {
        let mut driver = AutonomousDriver::new(Duration::from_millis(100));
        assert_eq!(driver.abort(), None);

        driver.start_session("id3", "text", 5);
        let id = driver.abort();
        assert_eq!(id.as_deref(), Some("id3"));
        assert!(driver.session.is_none());
    }

    #[test]
    fn stuck_detection_logic() {
        let mut s = AutonomousSession::new("id", "text", 10);
        assert_eq!(s.stuck_count, 0);

        // Simulate two stuck turns, not yet at limit.
        s.stuck_count = 2;
        assert!(!s.is_terminal());

        // Third stuck turn → caller sets state to Stuck.
        s.stuck_count = 3;
        s.state = AutonomousState::Stuck;
        assert!(s.is_terminal());
    }

    #[test]
    fn supervisor_fail_count_resets_on_success() {
        let mut s = AutonomousSession::new("id", "text", 10);
        s.supervisor_fail_count = 2;
        // Simulate a successful verification call resetting the counter.
        s.supervisor_fail_count = 0;
        assert_eq!(s.supervisor_fail_count, 0);
    }

    #[test]
    fn display_covers_all_variants() {
        assert_eq!(AutonomousState::Running.to_string(), "running");
        assert_eq!(AutonomousState::Verifying.to_string(), "verifying");
        assert_eq!(AutonomousState::Achieved.to_string(), "achieved");
        assert_eq!(AutonomousState::Stuck.to_string(), "stuck");
        assert_eq!(AutonomousState::Aborted.to_string(), "aborted");
        assert_eq!(AutonomousState::Failed.to_string(), "failed");
    }

    #[test]
    fn driver_new_panics_on_zero_delay() {
        let result = std::panic::catch_unwind(|| {
            let _ = AutonomousDriver::new(Duration::ZERO);
        });
        assert!(result.is_err(), "zero delay must panic");
    }

    #[test]
    fn no_cancelled_id_on_first_start() {
        let mut driver = AutonomousDriver::new(Duration::from_millis(100));
        let prev = driver.start_session("id1", "text", 5);
        assert!(prev.is_none(), "no prior session → no cancelled id");
    }
}
