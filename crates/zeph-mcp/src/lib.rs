//! MCP client lifecycle, tool discovery, and execution.

pub mod client;
pub mod error;
pub mod executor;
pub mod manager;
pub mod prompt;
pub mod registry;
pub mod security;
pub mod tool;

pub use error::McpError;
pub use executor::McpToolExecutor;
pub use manager::{McpManager, McpTransport, ServerEntry};
pub use prompt::format_mcp_tools_prompt;
pub use registry::McpToolRegistry;
pub use tool::McpTool;
