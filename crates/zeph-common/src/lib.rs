// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared utility functions and security primitives for Zeph crates.
//!
//! This crate provides pure utility functions (text manipulation, network helpers,
//! sanitization primitives), security primitives (`Secret`, `VaultError`), and
//! strongly-typed identifiers (`ToolName`, `SessionId`) that are needed by multiple crates.
//! It has no `zeph-*` dependencies. The optional `treesitter` feature adds tree-sitter
//! query constants and helpers.

pub mod config;
pub mod error_taxonomy;
pub mod fs_secure;
pub mod hash;
pub mod math;
pub mod memory;
pub mod net;
pub mod patterns;
pub mod policy;
pub mod quarantine;
pub mod sanitize;
pub mod secret;
pub mod security_event;
pub mod spawner;
pub mod task_supervisor;
pub mod text;
pub mod trust_level;
pub mod types;

/// Prefix embedded in tool output bodies when the full output was stored externally.
///
/// Format: `[full output stored — ID: {uuid} — {bytes} bytes, use read_overflow tool to retrieve]`
pub const OVERFLOW_NOTICE_PREFIX: &str = "[full output stored \u{2014} ID: ";

pub use math::{EmbeddingVector, Normalized, Unnormalized};
pub use policy::{PolicyLlmClient, PolicyMessage, PolicyRole};
pub use security_event::SecurityEventCategory;
pub use spawner::BlockingSpawner;
pub use task_supervisor::{
    BlockingError, BlockingHandle, MAX_RESTART_DELAY, RestartPolicy, TaskDescriptor, TaskHandle,
    TaskSnapshot, TaskStatus, TaskSupervisor,
};
pub use trust_level::SkillTrustLevel;
pub use types::{ProviderName, SessionId, SkillName, ToolDefinition, ToolName};

#[cfg(feature = "treesitter")]
pub mod treesitter;
