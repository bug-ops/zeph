// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-turn execution context shared across agent phases.

use tokio_util::sync::CancellationToken;
use zeph_config::security::TimeoutConfig;

/// Monotonically increasing per-conversation turn identifier.
///
/// Moved from `zeph-core` to `zeph-context` so [`TurnContext`] can be defined here
/// without creating a forbidden `zeph-context → zeph-core` dependency.
///
/// `TurnId(0)` is the first turn in a conversation. Values are strictly increasing by 1.
/// The counter resets to 0 when a new conversation starts (e.g., via `/new`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TurnId(pub u64);

impl TurnId {
    /// Return the next turn ID in sequence.
    ///
    /// Saturates at `u64::MAX` rather than wrapping or panicking.
    #[must_use]
    pub fn next(self) -> TurnId {
        TurnId(self.0.saturating_add(1))
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Per-turn execution context shared across phases (`loop`, `compose`, `persist`).
///
/// `TurnContext` is `Send + 'static` and cheaply cloneable so it can be passed by value
/// into subsystems that may outlive a `&mut Turn` borrow (background tasks, sub-services
/// extracted to other crates in Phase 2 of the agent decomposition).
///
/// It carries only data that is (a) immutable for the duration of the turn or (b)
/// intrinsically `Send + Clone` (the cancellation token).
///
/// # Examples
///
/// ```
/// use zeph_context::turn_context::{TurnContext, TurnId};
/// use zeph_config::security::TimeoutConfig;
/// use tokio_util::sync::CancellationToken;
///
/// let ctx = TurnContext::new(TurnId(0), CancellationToken::new(), TimeoutConfig::default());
/// assert_eq!(ctx.id, TurnId(0));
/// ```
#[derive(Debug, Clone)]
pub struct TurnContext {
    /// Monotonically increasing identifier for this turn within the conversation.
    pub id: TurnId,
    /// Per-turn cancellation token. A fresh token is created in `Agent::begin_turn`.
    /// Cancelled when the user aborts the turn or the agent shuts down.
    pub cancel_token: CancellationToken,
    /// Effective timeout configuration snapshotted at the start of the turn.
    ///
    /// Snapshotting (rather than reading from a shared config) ensures the turn's
    /// timeout policy is stable even if the live config is reloaded mid-turn.
    pub timeouts: TimeoutConfig,
    /// Optional channel-scoped tool allowlist for this turn.
    ///
    /// `None` means no channel-level restriction applies (other layers may still gate tool
    /// access). Always `None` until Phase 2 wires channel config into the agent runtime.
    ///
    /// TODO(#3498): populate from active channel config during Phase 2 crate extraction.
    pub tool_allowlist: Option<Vec<String>>,
}

impl TurnContext {
    /// Create a new `TurnContext`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_context::turn_context::{TurnContext, TurnId};
    /// use zeph_config::security::TimeoutConfig;
    /// use tokio_util::sync::CancellationToken;
    ///
    /// let ctx = TurnContext::new(TurnId(1), CancellationToken::new(), TimeoutConfig::default());
    /// assert_eq!(ctx.id, TurnId(1));
    /// ```
    #[must_use]
    pub fn new(id: TurnId, cancel_token: CancellationToken, timeouts: TimeoutConfig) -> Self {
        Self {
            id,
            cancel_token,
            timeouts,
            tool_allowlist: None,
        }
    }
}

const _: () = {
    fn assert_send_static<T: Send + 'static>() {}
    fn check() {
        assert_send_static::<TurnContext>();
        assert_send_static::<TurnId>();
    }
    let _ = check;
};

#[cfg(test)]
mod tests {
    use tokio_util::sync::CancellationToken;
    use zeph_config::security::TimeoutConfig;

    use super::*;

    #[test]
    fn turn_id_next_increments() {
        assert_eq!(TurnId(3).next(), TurnId(4));
    }

    #[test]
    fn turn_id_next_saturates_at_max() {
        assert_eq!(TurnId(u64::MAX).next(), TurnId(u64::MAX));
    }

    #[test]
    fn turn_id_display() {
        assert_eq!(TurnId(42).to_string(), "42");
    }

    #[test]
    fn turn_context_new_fields() {
        let token = CancellationToken::new();
        let ctx = TurnContext::new(TurnId(1), token.clone(), TimeoutConfig::default());
        assert_eq!(ctx.id, TurnId(1));
        assert!(ctx.tool_allowlist.is_none());
    }

    #[test]
    fn turn_context_clone_shares_cancel_token() {
        let ctx = TurnContext::new(
            TurnId(0),
            CancellationToken::new(),
            TimeoutConfig::default(),
        );
        let cloned = ctx.clone();
        ctx.cancel_token.cancel();
        assert!(cloned.cancel_token.is_cancelled());
    }
}
