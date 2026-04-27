// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Agent tool dispatcher for Zeph.
//!
//! This crate provides the [`AgentChannel`] trait, borrowed event carriers, and doom-loop
//! detection utilities used by the tool dispatch loop in `zeph-core`.
//!
//! # Architecture
//!
//! `zeph-agent-tools` does **not** depend on `zeph-core` or `zeph-channels`. It defines its
//! own minimal [`AgentChannel`] trait (sealed) which `zeph-core` implements via a local adapter
//! type `AgentChannelView<'a, C>`. This avoids the circular dependency that would arise from
//! using `zeph-core::channel::Channel` directly.
//!
//! # Crate status
//!
//! Phase 2 scaffolding (issue #3516). The `AgentChannel` trait and borrowed event carriers
//! are complete. Full `ToolDispatcher` extraction from `zeph-core` is tracked as a follow-up
//! once the persistence extraction (#3515) lands and integration tests are stable.

pub mod channel;
pub mod doom_loop;
pub mod error;
#[doc(hidden)]
pub mod sealed;

pub use channel::{AgentChannel, ChannelSinkError, ToolEventOutput, ToolEventStart};
pub use doom_loop::doom_loop_hash;
pub use error::ToolDispatchError;
pub use sealed::Sealed;
