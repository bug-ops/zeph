// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod command;
pub mod def;
pub mod error;
pub mod filter;
pub mod grants;
pub mod manager;
pub mod state;

pub use command::AgentCommand;
pub use def::{SkillFilter, SubAgentDef, SubAgentPermissions, ToolPolicy};
pub use error::SubAgentError;
pub use filter::{FilteredToolExecutor, filter_skills};
pub use grants::{Grant, GrantKind, PermissionGrants, SecretRequest};
pub use manager::{SubAgentHandle, SubAgentManager, SubAgentStatus};
pub use state::SubAgentState;
