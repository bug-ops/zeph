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
//! own minimal [`AgentChannel`] trait (sealed), letting a future adapter in `zeph-core` bridge
//! to `zeph-core::channel::Channel` without a circular dependency.
//!
//! # Crate status
//!
//! Phase 2 scaffolding (issue #3516, closed). The `AgentChannel` trait and borrowed event
//! carriers are complete, but no `zeph-core` adapter currently implements them and no
//! `ToolDispatcher` extraction has landed. Re-opening this work requires a new tracking issue.

pub mod channel;
pub mod doom_loop;
pub mod error;
#[doc(hidden)]
pub mod sealed;

pub use channel::{AgentChannel, ChannelSinkError, ToolEventOutput, ToolEventStart};
pub use doom_loop::doom_loop_hash;
pub use error::ToolDispatchError;
pub use sealed::Sealed;
