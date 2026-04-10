// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Subagent management: spawning, grants, transcripts, and lifecycle hooks.
//!
//! `zeph-subagent` provides the full lifecycle of sub-agent tasks within the Zeph agent
//! framework. It covers:
//!
//! - **Definitions** ([`SubAgentDef`]) — parse YAML/TOML frontmatter from `.md` files,
//!   validate names and permissions, and load from priority-ordered directories.
//! - **Manager** ([`SubAgentManager`]) — spawn, cancel, collect, and resume sub-agent tasks
//!   against a configurable concurrency limit.
//! - **Grants** ([`PermissionGrants`]) — zero-trust TTL-bounded permission tracking for
//!   vault secrets and runtime tool grants.
//! - **Hooks** ([`fire_hooks`]) — run shell commands at `PreToolUse`, `PostToolUse`,
//!   `SubagentStart`, and `SubagentStop` lifecycle events.
//! - **Filter** ([`FilteredToolExecutor`]) — enforce per-agent [`ToolPolicy`] and denylist
//!   on every tool call before it reaches the real executor.
//! - **Transcripts** ([`TranscriptWriter`], [`TranscriptReader`]) — persist conversation
//!   history to JSONL files for session resume and auditing.
//! - **Memory** ([`ensure_memory_dir`], [`load_memory_content`]) — resolve and inject
//!   persistent `MEMORY.md` content into the sub-agent system prompt.
//! - **Commands** ([`AgentCommand`], [`AgentsCommand`]) — typed parsers for `/agent` and
//!   `/agents` slash commands.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use zeph_subagent::{SubAgentDef, SubAgentManager};
//!
//! // Parse a definition from markdown frontmatter.
//! let content = "---\nname: helper\ndescription: A helpful sub-agent\n---\nYou are a helper.\n";
//! let def = SubAgentDef::parse(content).expect("valid definition");
//! assert_eq!(def.name, "helper");
//!
//! // Create a manager with a concurrency limit of 4.
//! let _manager = SubAgentManager::new(4);
//! ```

mod agent_loop;
pub mod command;
pub mod def;
pub mod error;
pub mod filter;
pub mod grants;
pub mod hooks;
pub mod manager;
pub mod memory;
pub mod resolve;
pub mod state;
pub mod transcript;

pub use command::{AgentCommand, AgentsCommand};
pub use def::{
    MemoryScope, ModelSpec, PermissionMode, SkillFilter, SubAgentDef, SubAgentPermissions,
    ToolPolicy, is_valid_agent_name,
};
pub use error::SubAgentError;
pub use filter::{FilteredToolExecutor, PlanModeExecutor, filter_skills};
pub use grants::{Grant, GrantKind, PermissionGrants, SecretRequest};
pub use hooks::{
    HookDef, HookError, HookMatcher, HookType, SubagentHooks, fire_hooks, matching_hooks,
};
pub use manager::{SpawnContext, SubAgentHandle, SubAgentManager, SubAgentStatus};
pub use memory::{ensure_memory_dir, load_memory_content};
pub use resolve::resolve_agent_paths;
pub use state::SubAgentState;
pub use transcript::{TranscriptMeta, TranscriptReader, TranscriptWriter, sweep_old_transcripts};
