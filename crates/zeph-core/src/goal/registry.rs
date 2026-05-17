// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared registry for active autonomous goal sessions.
//!
//! [`AutonomousRegistry`] is an `Arc<Mutex<_>>` wrapper around a `HashMap` keyed by goal ID.
//! It is cloned into the `/agents` fleet view handler so it can read session state without
//! borrowing `Agent` directly. At most one session is active at any time (invariant A1), so
//! the map will virtually always contain 0 or 1 entries.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tracing::error;

use super::autonomous::{AutonomousState, SupervisorVerdict};

/// Snapshot of one autonomous session suitable for cross-crate display.
///
/// This is a value-type copy — the registry retains the live session. Constructed by
/// [`AutonomousRegistry::list`].
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    /// Goal ID (UUID string).
    pub goal_id: String,
    /// First 80 characters of goal text (for display).
    pub goal_text_short: String,
    /// Current session state.
    pub state: AutonomousState,
    /// Number of turns executed so far.
    pub turns_executed: u32,
    /// Maximum turns for this session.
    pub max_turns: u32,
    /// Wall-clock time elapsed since session start.
    pub elapsed: std::time::Duration,
    /// Last supervisor verdict, if any.
    pub last_verdict: Option<SupervisorVerdict>,
}

/// Shared registry of autonomous goal sessions.
///
/// Cloned freely — all clones share the same underlying `Mutex<HashMap>`. The agent owns one
/// clone; the `/agents` handler owns another.
///
/// # Example
///
/// ```rust
/// use zeph_core::goal::registry::AutonomousRegistry;
///
/// let reg = AutonomousRegistry::default();
/// let snapshots = reg.list();
/// assert!(snapshots.is_empty());
/// ```
#[derive(Clone, Default)]
pub struct AutonomousRegistry {
    inner: Arc<Mutex<HashMap<String, RegistryEntry>>>,
}

struct RegistryEntry {
    goal_text: String,
    state: AutonomousState,
    turns_executed: u32,
    max_turns: u32,
    started_at: Instant,
    last_verdict: Option<SupervisorVerdict>,
}

impl AutonomousRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or update a session entry.
    ///
    /// All fields are taken as a flat argument list; the registry owns the produced entry.
    /// The lock is never held across an `.await`.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert(
        &self,
        goal_id: impl Into<String>,
        goal_text: impl Into<String>,
        state: AutonomousState,
        turns_executed: u32,
        max_turns: u32,
        started_at: Instant,
        last_verdict: Option<SupervisorVerdict>,
    ) {
        let goal_id = goal_id.into();
        let entry = RegistryEntry {
            goal_text: goal_text.into(),
            state,
            turns_executed,
            max_turns,
            started_at,
            last_verdict,
        };
        let mut map = self.inner.lock().unwrap_or_else(|e| {
            error!("AutonomousRegistry::upsert: mutex poisoned, recovering guard");
            e.into_inner()
        });
        map.insert(goal_id, entry);
    }

    /// Remove a session entry.
    pub fn remove(&self, goal_id: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|e| {
            error!("AutonomousRegistry::remove: mutex poisoned, recovering guard");
            e.into_inner()
        });
        map.remove(goal_id);
    }

    /// Return snapshots for all sessions currently in the registry.
    ///
    /// In practice this is 0 or 1 entries (invariant A1).
    #[must_use]
    pub fn list(&self) -> Vec<SessionSnapshot> {
        let map = self.inner.lock().unwrap_or_else(|e| {
            error!("AutonomousRegistry::list: mutex poisoned, recovering guard");
            e.into_inner()
        });
        map.iter()
            .map(|(id, e)| {
                let short = if e.goal_text.len() > 80 {
                    let end = e.goal_text.floor_char_boundary(80);
                    format!("{}…", &e.goal_text[..end])
                } else {
                    e.goal_text.clone()
                };
                SessionSnapshot {
                    goal_id: id.clone(),
                    goal_text_short: short,
                    state: e.state,
                    turns_executed: e.turns_executed,
                    max_turns: e.max_turns,
                    elapsed: e.started_at.elapsed(),
                    last_verdict: e.last_verdict.clone(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_by_default() {
        let reg = AutonomousRegistry::new();
        assert!(reg.list().is_empty());
    }

    #[test]
    fn upsert_and_list() {
        let reg = AutonomousRegistry::new();
        reg.upsert(
            "id1",
            "do something useful",
            AutonomousState::Running,
            3,
            20,
            Instant::now(),
            None,
        );
        let list = reg.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].goal_id, "id1");
        assert_eq!(list[0].state, AutonomousState::Running);
        assert_eq!(list[0].turns_executed, 3);
    }

    #[test]
    fn remove_clears_entry() {
        let reg = AutonomousRegistry::new();
        reg.upsert(
            "id2",
            "text",
            AutonomousState::Running,
            0,
            5,
            Instant::now(),
            None,
        );
        assert_eq!(reg.list().len(), 1);
        reg.remove("id2");
        assert!(reg.list().is_empty());
    }

    #[test]
    fn long_goal_text_is_truncated() {
        let reg = AutonomousRegistry::new();
        let long_text = "a".repeat(200);
        reg.upsert(
            "id3",
            long_text,
            AutonomousState::Running,
            0,
            5,
            Instant::now(),
            None,
        );
        let snap = &reg.list()[0];
        assert!(
            snap.goal_text_short.len() <= 84,
            "truncation should keep display short"
        );
    }

    #[test]
    fn upsert_overwrites_existing_entry() {
        let reg = AutonomousRegistry::new();
        reg.upsert(
            "id5",
            "original text",
            AutonomousState::Running,
            1,
            10,
            Instant::now(),
            None,
        );
        reg.upsert(
            "id5",
            "updated text",
            AutonomousState::Verifying,
            5,
            10,
            Instant::now(),
            None,
        );
        let list = reg.list();
        assert_eq!(list.len(), 1, "upsert should not create duplicates");
        assert_eq!(list[0].state, AutonomousState::Verifying);
        assert_eq!(list[0].turns_executed, 5);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let reg = AutonomousRegistry::new();
        reg.remove("does-not-exist"); // must not panic
        assert!(reg.list().is_empty());
    }

    #[test]
    fn clone_shares_state() {
        let reg = AutonomousRegistry::new();
        let reg2 = reg.clone();
        reg.upsert(
            "id4",
            "shared",
            AutonomousState::Running,
            0,
            5,
            Instant::now(),
            None,
        );
        assert_eq!(reg2.list().len(), 1, "clone should see the same data");
    }
}
