// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::cell::RefCell;
use std::rc::Rc;

use agent_client_protocol as acp;

#[cfg(feature = "acp-http")]
pub mod auth;
pub mod bridge;
#[cfg(feature = "acp-http")]
pub mod discovery;
pub mod http;
pub mod router;
pub mod stdio;
pub mod ws;

#[cfg(test)]
mod tests;

pub use stdio::{serve_connection, serve_stdio};

#[cfg(feature = "acp-http")]
pub use http::AcpHttpState;
#[cfg(feature = "acp-http")]
pub use router::acp_router;

/// Shared slot populated after `AgentSideConnection::new` so `new_session` can access
/// the connection to build ACP tool adapters.
pub(crate) type ConnSlot = Rc<RefCell<Option<Rc<acp::AgentSideConnection>>>>;

/// Configuration for the ACP server passed through to the agent.
pub struct AcpServerConfig {
    pub agent_name: String,
    pub agent_version: String,
    pub max_sessions: usize,
    pub session_idle_timeout_secs: u64,
    pub permission_file: Option<std::path::PathBuf>,
    /// Optional factory for runtime model switching.
    pub provider_factory: Option<crate::agent::ProviderFactory>,
    /// Available model identifiers to advertise in `new_session`.
    pub available_models: Vec<String>,
    /// Optional shared MCP manager for `ext_method` add/remove/list.
    pub mcp_manager: Option<std::sync::Arc<zeph_mcp::McpManager>>,
    /// Bearer token for HTTP and WebSocket transport authentication.
    /// When `Some`, all /acp and /acp/ws requests must include the token.
    pub auth_bearer_token: Option<String>,
    /// Whether to serve the /.well-known/acp.json discovery manifest.
    pub discovery_enabled: bool,
    /// Timeout in seconds for terminal command execution before kill is sent.
    pub terminal_timeout_secs: u64,
}

impl Clone for AcpServerConfig {
    fn clone(&self) -> Self {
        Self {
            agent_name: self.agent_name.clone(),
            agent_version: self.agent_version.clone(),
            max_sessions: self.max_sessions,
            session_idle_timeout_secs: self.session_idle_timeout_secs,
            permission_file: self.permission_file.clone(),
            provider_factory: self.provider_factory.clone(),
            available_models: self.available_models.clone(),
            mcp_manager: self.mcp_manager.clone(),
            auth_bearer_token: self.auth_bearer_token.clone(),
            discovery_enabled: self.discovery_enabled,
            terminal_timeout_secs: self.terminal_timeout_secs,
        }
    }
}

impl Default for AcpServerConfig {
    fn default() -> Self {
        Self {
            agent_name: String::new(),
            agent_version: String::new(),
            max_sessions: 4,
            session_idle_timeout_secs: 1800,
            permission_file: None,
            provider_factory: None,
            available_models: Vec::new(),
            mcp_manager: None,
            auth_bearer_token: None,
            discovery_enabled: true,
            terminal_timeout_secs: 120,
        }
    }
}
