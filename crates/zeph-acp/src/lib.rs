// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod agent;
pub(crate) mod custom;
pub mod error;
pub mod fs;
pub mod lsp;
pub mod mcp_bridge;
pub mod permission;
pub mod terminal;
pub mod transport;

pub use agent::{AcpContext, AgentSpawner, ProviderFactory};
pub use error::AcpError;
pub use fs::AcpFileExecutor;
pub use lsp::{AcpLspProvider, DiagnosticsCache, LspProvider};
pub use mcp_bridge::acp_mcp_servers_to_entries;
pub use permission::AcpPermissionGate;
pub use terminal::AcpShellExecutor;
pub use transport::{AcpServerConfig, serve_connection, serve_stdio};

#[cfg(feature = "acp-http")]
pub use agent::SendAgentSpawner;
#[cfg(feature = "acp-http")]
pub use transport::{AcpHttpState, acp_router};
