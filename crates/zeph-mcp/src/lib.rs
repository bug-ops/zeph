// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MCP client lifecycle, tool discovery, and execution.

pub mod caller;
pub mod client;
pub mod error;
pub mod executor;
pub mod manager;
pub mod oauth;
pub mod policy;
pub mod prompt;
pub mod registry;
pub mod sanitize;
pub mod security;
pub mod tool;

#[cfg(test)]
pub mod testing;

#[cfg(feature = "mock")]
pub mod mock;

pub use caller::McpCaller;
pub use client::{OAuthConnectResult, OAuthPending, ToolRefreshEvent};
pub use error::McpError;
pub use executor::McpToolExecutor;
pub use manager::{McpManager, McpTransport, McpTrustLevel, ServerEntry};
#[cfg(feature = "mock")]
pub use mock::{McpCall, MockMcpCaller};
pub use policy::{McpPolicy, PolicyEnforcer, PolicyViolation, RateLimit};
pub use prompt::format_mcp_tools_prompt;
pub use registry::McpToolRegistry;
pub use tool::McpTool;
