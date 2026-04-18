// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Agent run-loop event discriminator.
//!
//! [`LoopEvent`] unifies all event sources polled inside `Agent::run()` into
//! a single enum so the main loop body stays readable and each event source
//! gets a dedicated handler method.

use crate::agent::task_injection::TaskInjection;
use crate::channel::ChannelMessage;
use crate::file_watcher::FileChangedEvent;

/// One event yielded by [`super::Agent::next_event`] per `tokio::select!` cycle.
///
/// Each variant corresponds to exactly one branch of the `tokio::select!` block
/// in the agent run loop. The discriminator is `pub(crate)` — it is an internal
/// implementation detail with no public API impact.
pub(crate) enum LoopEvent {
    /// An inbound message arrived on the agent channel.
    Message(ChannelMessage),

    /// A graceful shutdown signal was received.
    Shutdown,

    /// The skill registry was hot-reloaded; skills must be refreshed.
    SkillReload,

    /// The system instructions file changed; instructions must be reloaded.
    InstructionReload,

    /// The configuration file changed; config must be reloaded.
    ConfigReload,

    /// An update notification string should be forwarded to the channel.
    UpdateNotification(String),

    /// A background experiment completed; the result message should be forwarded.
    ExperimentCompleted(String),

    /// A scheduler-injected prompt should be processed as a new agent turn.
    ScheduledTask(String),

    /// A prompt injected by the `/loop` command on a tick.
    TaskInjected(TaskInjection),

    /// A watched file changed and the agent should react accordingly.
    FileChanged(FileChangedEvent),
}
