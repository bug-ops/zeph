pub mod channel;
pub mod def;
pub mod error;
pub mod filter;
pub mod grants;
pub mod manager;

pub use channel::{A2aMessage, AgentHalf, OrchestratorHalf, new_channel};
pub use def::{SkillFilter, SubAgentDef, SubAgentPermissions, ToolPolicy};
pub use error::SubAgentError;
pub use filter::{FilteredToolExecutor, filter_skills};
pub use grants::{Grant, GrantKind, PermissionGrants, SecretRequest};
pub use manager::{SubAgentHandle, SubAgentManager, SubAgentStatus};
