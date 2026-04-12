// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Slash command handler implementations.
//!
//! Each module contains one or more handler structs implementing [`CommandHandler<CommandContext>`].
//! Handlers access agent subsystems through the trait objects on [`CommandContext`].
//!
//! [`CommandHandler<CommandContext>`]: crate::CommandHandler
//! [`CommandContext`]: crate::context::CommandContext

pub mod agent_cmd;
pub mod compaction;
pub mod debug;
pub mod experiment;
pub mod help;
pub mod lsp;
pub mod mcp;
pub mod memory;
pub mod misc;
pub mod model;
pub mod plan;
pub mod policy;
pub mod scheduler;
pub mod session;
pub mod status;
// Note: skill, skills, feedback handlers are kept as TODO — they hold non-Send DB references
// across .await points which prevents implementing AgentAccess::handle_skill as Send future.
// These commands continue to be dispatched via dispatch_slash_command in zeph-core until
// SemanticMemory and AnyProvider implement Sync.
pub mod skill;
