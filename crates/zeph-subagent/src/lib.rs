// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Subagent management: spawning, grants, transcripts, and lifecycle hooks.

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
