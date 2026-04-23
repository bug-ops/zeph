// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ACP (Agent Client Protocol) server for IDE embedding.
//!
//! `zeph-acp` exposes the Zeph agent over the Agent Client Protocol so that
//! IDEs such as Zed can connect to it as a first-class AI assistant.
//!
//! # Architecture
//!
//! ```text
//! IDE / client
//!   │  JSON-RPC over stdio / HTTP-SSE / WebSocket
//!   ▼
//! transport  ──►  ZephAcpAgent (ACP SDK Agent impl)
//!                  │
//!                  ├─ AgentSpawner  ──►  agent loop (LoopbackChannel)
//!                  ├─ AcpPermissionGate  ──►  IDE tool-call approval
//!                  ├─ AcpFileExecutor   ──►  IDE fs/* proxying
//!                  ├─ AcpShellExecutor  ──►  IDE terminal/* proxying
//!                  └─ AcpLspProvider    ──►  IDE LSP ext_method proxying
//! ```
//!
//! # Transports
//!
//! | Transport | Entry point | Feature flag |
//! |-----------|-------------|--------------|
//! | stdio (default) | [`serve_stdio`] | always |
//! | HTTP + SSE | [`acp_router`] | `acp-http` |
//! | WebSocket | [`acp_router`] | `acp-http` |
//!
//! # Feature flags
//!
//! | Flag | Description |
//! |------|-------------|
//! | `acp-http` | HTTP/SSE and WebSocket transports via axum |
//! | `unstable-session-close` | ACP session close extension |
//! | `unstable-session-fork` | ACP session fork extension |
//! | `unstable-session-resume` | ACP session resume extension |
//! | `unstable-session-usage` | ACP session token-usage extension |
//! | `unstable-session-model` | ACP session model-switching extension |
//! | `unstable-elicitation` | ACP elicitation schema types |
//! | `unstable-logout` | ACP logout extension |
//!
//! # Quick start (stdio)
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use parking_lot::RwLock;
//! use zeph_acp::{AgentSpawner, AcpServerConfig, serve_stdio};
//!
//! # async fn run() -> Result<(), zeph_acp::AcpError> {
//! let spawner: AgentSpawner = Arc::new(|channel, ctx, session| {
//!     Box::pin(async move {
//!         // run your agent loop here
//!         drop((channel, ctx, session));
//!     })
//! });
//!
//! let config = AcpServerConfig {
//!     agent_name: "my-agent".to_owned(),
//!     agent_version: "0.1.0".to_owned(),
//!     ..AcpServerConfig::default()
//! };
//!
//! serve_stdio(spawner, config).await?;
//! # Ok(())
//! # }
//! ```

pub mod agent;
pub mod client;
pub(crate) mod custom;
pub mod error;
pub mod fs;
pub mod lsp;
pub mod mcp_bridge;
pub mod permission;
pub mod terminal;
pub mod transport;

pub use agent::{AcpContext, AgentSpawner, ProviderFactory, SessionContext, run_agent};
pub use client::{
    AcpClientError, RunOutcome, SubagentConfig, SubagentHandle, run_session, spawn_subagent,
};
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
