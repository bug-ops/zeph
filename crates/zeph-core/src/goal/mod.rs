// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Long-horizon goal lifecycle subsystem.
//!
//! A goal is a persistent user intent that spans multiple turns. At most one goal
//! can be `Active` at a time. The subsystem tracks token consumption per turn and
//! injects an `<active_goal>` block into the volatile system-prompt region.
//!
//! ## Architecture
//!
//! - [`GoalStatus`] — FSM states with valid transition table.
//! - [`Goal`] / [`GoalSnapshot`] — full DB row vs. lightweight cross-crate view.
//! - [`GoalStore`] — SQLite/Postgres-backed persistence with transactional `create()`.
//! - [`GoalAccounting`] — per-turn token accounting service.
//!
//! ## Invariants
//!
//! - **G1**: At most one `Active` goal per database. Enforced jointly by the partial
//!   unique index and the transactional `GoalStore::create`.
//! - **G2**: Stale transitions return [`GoalError::StaleUpdate`]; handlers refetch silently.
//! - **G3**: `<active_goal>` block appears only after `<!-- cache:volatile -->` in the
//!   system prompt. Enforced by a snapshot test in `context/assembly.rs`.
//! - **G4**: `GoalAccounting::on_turn_complete` is fire-and-forget. DB write failures
//!   log a `WARN` and never abort the turn.

mod accounting;
mod state;
pub mod store;

pub use accounting::GoalAccounting;
pub use state::GoalStatus;
pub use store::{GoalError, GoalStore};

use chrono::{DateTime, Utc};

/// A persisted long-horizon goal row.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Goal {
    /// Unique identifier (UUID v4 string).
    pub id: String,
    /// User-provided goal text (max `[goals] max_text_chars` chars at creation time).
    pub text: String,
    /// Current FSM status.
    pub status: GoalStatus,
    /// Optional token budget. `None` means unlimited.
    pub token_budget: Option<i64>,
    /// Number of conversation turns completed under this goal.
    pub turns_used: i64,
    /// Total tokens consumed across all turns under this goal.
    pub tokens_used: i64,
    /// When the goal was created.
    pub created_at: DateTime<Utc>,
    /// Last mutation time; used as a CAS guard for stale-update detection.
    pub updated_at: DateTime<Utc>,
    /// When the goal reached `Completed` or `Cleared`. `None` while active or paused.
    pub completed_at: Option<DateTime<Utc>>,
}

/// Lightweight, cross-crate snapshot of an active goal.
///
/// Carries only what TUI and command handlers need, without pulling `zeph-core` into `zeph-tui`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GoalSnapshot {
    /// UUID string of the goal.
    pub id: String,
    /// Goal text, pre-validated to fit within `max_text_chars`.
    pub text: String,
    /// Current FSM status.
    pub status: GoalStatus,
    /// Number of turns completed.
    pub turns_used: u64,
    /// Total tokens consumed.
    pub tokens_used: u64,
    /// Optional token budget (`None` = unlimited).
    pub token_budget: Option<u64>,
}

impl From<Goal> for GoalSnapshot {
    fn from(g: Goal) -> Self {
        Self {
            id: g.id,
            text: g.text,
            status: g.status,
            turns_used: g.turns_used.max(0).cast_unsigned(),
            tokens_used: g.tokens_used.max(0).cast_unsigned(),
            token_budget: g.token_budget.map(|b| b.max(0).cast_unsigned()),
        }
    }
}
