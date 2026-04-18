// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Task injection from the `/loop` command into the agent run-loop.

/// A raw user prompt injected by the `/loop` command on each tick.
///
/// Carried by [`LoopEvent::TaskInjected`] and dispatched without any prefix
/// (unlike scheduler tasks which prepend [`SCHEDULED_TASK_PREFIX`]).
///
/// [`LoopEvent::TaskInjected`]: super::loop_event::LoopEvent::TaskInjected
/// [`SCHEDULED_TASK_PREFIX`]: super::SCHEDULED_TASK_PREFIX
pub(crate) struct TaskInjection {
    /// The prompt text to inject as a new agent turn.
    pub(crate) prompt: String,
}
