// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sub-trait definitions for command handler context access.
//!
//! Each trait exposes the minimal interface a handler needs to access one subsystem.
//! `zeph-core` implements these traits on its internal state types and constructs
//! [`CommandContext`] at dispatch time.
//!
//! [`CommandContext`]: crate::context::CommandContext

pub mod agent;
pub mod channel;
pub mod debug;
pub mod graph;
pub mod integration;
pub mod lsp;
pub mod mcp;
pub mod memory;
pub mod messages;
pub mod misc;
pub mod model;
pub mod orchestration;
pub mod policy;
pub mod scheduler;
pub mod session;
pub mod session_control;
pub mod skill;
pub mod subagent;
pub mod tracking;
pub mod worktree;
