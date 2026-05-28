// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! State types shared between the autonomous goal subsystem and command handlers.

use serde::{Deserialize, Serialize};

/// FSM states for an autonomous goal session.
///
/// Valid transitions:
/// - `Running` → `Verifying` (supervisor check triggered)
/// - `Verifying` → `Running` (supervisor: not yet achieved)
/// - `Verifying` → `Achieved` (supervisor: goal met)
/// - `Running` / `Verifying` → `Stuck` (stuck counter exhausted or supervisor failures)
/// - `Running` / `Verifying` → `Aborted` (user cancel or turn limit reached)
/// - `Running` / `Verifying` → `Failed` (unrecoverable error)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
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
