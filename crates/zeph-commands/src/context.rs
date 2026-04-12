// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Non-generic command execution context.
//!
//! [`CommandContext`] is the single argument passed to every [`CommandHandler`]. It provides
//! access to agent subsystems through trait objects, eliminating the `C: Channel` generic from
//! `CommandHandler` and `CommandRegistry`.
//!
//! `zeph-core` constructs a `CommandContext` at dispatch time from `Agent<C>` fields:
//!
//! ```rust,ignore
//! let mut ctx = CommandContext {
//!     sink:     &mut sink_adapter,
//!     debug:    &mut self.debug_state,
//!     messages: &mut messages_impl,
//!     session:  &session_impl,
//!     agent:    &mut agent_impl,
//! };
//! registry.dispatch(&mut ctx, input).await;
//! ```
//!
//! [`CommandHandler`]: crate::CommandHandler

use crate::sink::ChannelSink;
use crate::traits::agent::AgentAccess;
use crate::traits::debug::DebugAccess;
use crate::traits::messages::MessageAccess;
use crate::traits::session::SessionAccess;

/// Typed access to agent subsystems for slash command handlers.
///
/// Each field is a trait object providing access to one subsystem group. Constructed by
/// `zeph-core` at dispatch time from `Agent<C>` fields. Handlers receive `&mut CommandContext`
/// and access only the fields they need.
///
/// # Lifetimes
///
/// The lifetime `'a` ties all references to the dispatch scope. A `CommandContext` must not
/// outlive the `&mut Agent<C>` it was constructed from.
pub struct CommandContext<'a> {
    /// I/O channel for sending responses to the user.
    pub sink: &'a mut dyn ChannelSink,
    /// Debug/diagnostics state: dump, format, logging config.
    pub debug: &'a mut dyn DebugAccess,
    /// Conversation message history and queue operations.
    pub messages: &'a mut dyn MessageAccess,
    /// Session and channel properties (e.g., `supports_exit`).
    pub session: &'a dyn SessionAccess,
    /// Broad access to agent subsystems for commands that need multiple agent fields.
    pub agent: &'a mut dyn AgentAccess,
}
