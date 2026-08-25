// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Slash command handler implementations.
//!
//! Each module contains one or more handler structs implementing [`CommandHandler<CommandContext>`].
//! Handlers access agent subsystems through the trait objects on [`CommandContext`].
//!
//! [`CommandHandler<CommandContext>`]: crate::CommandHandler
//! [`CommandContext`]: crate::context::CommandContext

pub mod acp;
pub mod agent_cmd;
pub mod agents_fleet;
pub mod caveman;
pub mod cd;
pub mod checkpoint;
pub mod cocoon;
pub mod compaction;
pub mod conv;
pub mod debug;
pub mod experiment;
pub mod goal;
pub mod help;
pub mod loop_cmd;
pub mod lsp;
pub mod mcp;
pub mod memory;
pub mod misc;
pub mod model;
pub mod plan;
pub mod plugins;
pub mod policy;
pub mod reasoning_effort;
pub mod scheduler;
pub mod search;
pub mod session;
pub mod skill;
pub mod status;
#[cfg(test)]
pub mod test_helpers;
pub mod think_tokens;
pub mod trajectory;
pub mod worktree;
