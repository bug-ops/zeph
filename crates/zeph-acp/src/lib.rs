pub mod agent;
pub mod error;
pub mod fs;
pub mod mcp_bridge;
pub mod permission;
pub mod terminal;
pub mod transport;

pub use agent::{AcpContext, AgentSpawner};
pub use error::AcpError;
pub use fs::AcpFileExecutor;
pub use mcp_bridge::acp_mcp_servers_to_entries;
pub use permission::AcpPermissionGate;
pub use terminal::AcpShellExecutor;
pub use transport::serve_stdio;
