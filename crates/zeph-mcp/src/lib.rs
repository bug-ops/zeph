//! MCP client lifecycle, tool discovery, and execution.

pub mod client;
pub mod error;
pub mod executor;
pub mod manager;
pub mod prompt;
#[cfg(feature = "qdrant")]
pub mod registry;
pub mod tool;

pub use error::McpError;
pub use executor::McpToolExecutor;
pub use manager::{McpManager, McpTransport, ServerEntry};
pub use prompt::format_mcp_tools_prompt;
#[cfg(feature = "qdrant")]
pub use registry::McpToolRegistry;
pub use tool::McpTool;
