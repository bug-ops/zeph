// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ACP transport layer — stdio, HTTP+SSE, and WebSocket.
//!
//! Each transport variant drives the same [`ZephAcpAgent`] via the `agent-client-protocol`
//! JSON-RPC connection. The agent implementation is `!Send`, so every connection runs on a
//! dedicated current-thread tokio runtime inside an OS thread.
//!
//! # Entry points
//!
//! | Transport | Function/Type |
//! |-----------|---------------|
//! | stdio | [`serve_stdio`], [`serve_connection`] |
//! | HTTP + SSE | [`acp_router`] (feature `acp-http`) |
//! | WebSocket | part of [`acp_router`] (feature `acp-http`) |
//!
//! [`ZephAcpAgent`]: crate::agent::ZephAcpAgent

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

/// Startup readiness notification sent as the first stdio JSON-RPC frame.
///
/// When set in [`AcpServerConfig::ready_notification`], `serve_stdio` / `serve_connection`
/// emit a `zeph/ready` JSON-RPC notification **before** the ACP handshake so that
/// process supervisors (e.g. the Zed extension host) know the agent is alive and can
/// surface the log file path to the user.
#[derive(Clone, Debug)]
pub struct ReadyNotification {
    /// Semver version string of the running agent binary.
    pub version: String,
    /// PID of the agent process (for supervisor tracking).
    pub pid: u32,
    /// Absolute path to the log file, if file logging is configured.
    pub log_file: Option<String>,
}

/// Shared slot populated after `AgentSideConnection::new` so `new_session` can access
/// the connection to build ACP tool adapters.
pub(crate) type ConnSlot = Rc<RefCell<Option<Rc<acp::AgentSideConnection>>>>;

/// Thread-safe, shared list of available model identifiers advertised in `new_session`.
pub type SharedAvailableModels = std::sync::Arc<parking_lot::RwLock<Vec<String>>>;

/// Configuration for the ACP server, threaded through to the agent on every connection.
///
/// Construct with `AcpServerConfig::default()` and override the fields you need.
///
/// # Examples
///
/// ```
/// use zeph_acp::AcpServerConfig;
///
/// let config = AcpServerConfig {
///     agent_name: "zeph".to_owned(),
///     agent_version: "1.0.0".to_owned(),
///     max_sessions: 4,
///     ..AcpServerConfig::default()
/// };
///
/// assert_eq!(config.agent_name, "zeph");
/// assert_eq!(config.max_sessions, 4);
/// ```
pub struct AcpServerConfig {
    /// Display name of the agent reported to IDEs during handshake.
    pub agent_name: String,
    /// Semver version of the agent reported to IDEs during handshake.
    pub agent_version: String,
    /// Maximum number of concurrent ACP sessions (default: 4).
    pub max_sessions: usize,
    /// Seconds of inactivity before an idle session is reaped (default: 1800).
    pub session_idle_timeout_secs: u64,
    /// Path to the TOML permission file for tool-call approval decisions.
    ///
    /// Defaults to `$XDG_CONFIG_HOME/zeph/acp-permissions.toml` when `None`.
    pub permission_file: Option<std::path::PathBuf>,
    /// Optional factory for runtime model switching via `set_session_config_option`.
    pub provider_factory: Option<crate::agent::ProviderFactory>,
    /// Available model identifiers to advertise in `new_session` `config_options`.
    pub available_models: SharedAvailableModels,
    /// Optional shared MCP manager for `ext_method` add/remove/list.
    pub mcp_manager: Option<std::sync::Arc<zeph_mcp::McpManager>>,
    /// Bearer token for HTTP and WebSocket transport authentication.
    ///
    /// When `Some`, all `/acp` and `/acp/ws` requests must include the token in
    /// an `Authorization: Bearer <token>` header. When `None`, the endpoints are
    /// publicly accessible and a warning is logged at startup.
    pub auth_bearer_token: Option<String>,
    /// Whether to serve the `/.well-known/acp.json` discovery manifest.
    pub discovery_enabled: bool,
    /// Timeout in seconds for terminal command execution before the process is killed.
    pub terminal_timeout_secs: u64,
    /// Project rule file paths to advertise in `new_session` `_meta`.
    pub project_rules: Vec<std::path::PathBuf>,
    /// Maximum characters for auto-generated session titles (0 = no limit).
    pub title_max_chars: usize,
    /// Maximum number of sessions returned by list endpoints (0 = unlimited).
    pub max_history: usize,
    /// Path to the `SQLite` database for ACP session persistence.
    ///
    /// When set, the agent persists session events and loads conversation history
    /// from this database. When `None`, sessions are in-memory only.
    pub sqlite_path: Option<String>,
    /// Optional startup notification emitted as the first stdio JSON-RPC frame.
    pub ready_notification: Option<ReadyNotification>,
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
            project_rules: self.project_rules.clone(),
            title_max_chars: self.title_max_chars,
            max_history: self.max_history,
            sqlite_path: self.sqlite_path.clone(),
            ready_notification: self.ready_notification.clone(),
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
            available_models: std::sync::Arc::new(parking_lot::RwLock::new(Vec::new())),
            mcp_manager: None,
            auth_bearer_token: None,
            discovery_enabled: true,
            terminal_timeout_secs: 120,
            project_rules: Vec::new(),
            title_max_chars: 60,
            max_history: 100,
            sqlite_path: None,
            ready_notification: None,
        }
    }
}
