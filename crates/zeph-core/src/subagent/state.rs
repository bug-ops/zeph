// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Lifecycle state of a sub-agent task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentState {
    Submitted,
    Working,
    Completed,
    Failed,
    Canceled,
}
