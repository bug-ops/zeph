// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Goal status FSM with valid transition table.

/// Status of a long-horizon goal.
///
/// Transitions form a directed acyclic graph where `Completed` and `Cleared`
/// are terminal states. `/goal create` is NOT a transition — it inserts a new row.
///
/// ```text
/// Active ──► Paused ──► Active
///   │         │
///   ▼         ▼
/// Completed  Cleared
/// Active ──► Completed
/// Active ──► Cleared
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// Goal is being actively tracked and injected into context.
    Active,
    /// Goal is paused — not injected into context, resumable.
    Paused,
    /// Goal was marked as achieved. Terminal state.
    Completed,
    /// Goal was dismissed without completion. Terminal state.
    Cleared,
}

impl GoalStatus {
    /// Return whether `self → to` is a valid FSM transition.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_core::goal::GoalStatus;
    ///
    /// assert!(GoalStatus::Active.can_transition_to(GoalStatus::Paused));
    /// assert!(!GoalStatus::Completed.can_transition_to(GoalStatus::Active));
    /// ```
    #[must_use]
    pub fn can_transition_to(self, to: Self) -> bool {
        matches!(
            (self, to),
            (Self::Active, Self::Paused | Self::Completed | Self::Cleared)
                | (Self::Paused, Self::Active | Self::Cleared)
        )
    }

    /// Return `true` if this status accepts no further transitions.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_core::goal::GoalStatus;
    ///
    /// assert!(GoalStatus::Completed.is_terminal());
    /// assert!(!GoalStatus::Active.is_terminal());
    /// ```
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cleared)
    }

    /// Short ASCII symbol used in TUI status badge.
    #[must_use]
    pub fn badge_symbol(self) -> &'static str {
        match self {
            Self::Active => "▶",
            Self::Paused => "⏸",
            Self::Completed => "✓",
            Self::Cleared => "✗",
        }
    }
}

impl std::fmt::Display for GoalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Cleared => "cleared",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_transitions() {
        assert!(GoalStatus::Active.can_transition_to(GoalStatus::Paused));
        assert!(GoalStatus::Active.can_transition_to(GoalStatus::Completed));
        assert!(GoalStatus::Active.can_transition_to(GoalStatus::Cleared));
        assert!(GoalStatus::Paused.can_transition_to(GoalStatus::Active));
        assert!(GoalStatus::Paused.can_transition_to(GoalStatus::Cleared));
    }

    #[test]
    fn terminal_states_reject_transitions() {
        for from in [GoalStatus::Completed, GoalStatus::Cleared] {
            for to in [
                GoalStatus::Active,
                GoalStatus::Paused,
                GoalStatus::Completed,
                GoalStatus::Cleared,
            ] {
                assert!(
                    !from.can_transition_to(to),
                    "{from:?} -> {to:?} should be invalid"
                );
            }
        }
    }

    #[test]
    fn is_terminal() {
        assert!(GoalStatus::Completed.is_terminal());
        assert!(GoalStatus::Cleared.is_terminal());
        assert!(!GoalStatus::Active.is_terminal());
        assert!(!GoalStatus::Paused.is_terminal());
    }
}
