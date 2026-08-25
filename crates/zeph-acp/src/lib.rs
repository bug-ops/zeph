// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#![recursion_limit = "256"]

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
//! | HTTP + SSE | `acp_router` | `acp-http` |
//! | WebSocket | `acp_router` | `acp-http` |
//!
//! # Feature flags
//!
//! | Flag | Description |
//! |------|-------------|
//! | `acp-http` | HTTP/SSE and WebSocket transports via axum |
//! | `unstable-session-fork` | ACP session fork extension |
//! | `unstable-session-usage` | ACP session token-usage extension |
//! | `unstable-elicitation` | ACP elicitation schema types |
//! | `unstable-llm-providers` | ACP LLM provider listing extension |
//! | `unstable-auth-methods` | ACP auth-methods extension |
//! | `unstable-cancel-request` | Wires `$/cancel_request` onto the internal cancel signal (#5362) |
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

// TODO(critic): A-4 — evaluate channel-adapter collapse after spec 013 amendment for Send/!Send agent variant.

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

pub use agent::{
    AcpContext, AgentSpawner, ProviderFactory, SessionContext, SessionStatusNotifier, run_agent,
    warm_model_caches,
};
pub use client::{
    AcpClientError, RunOutcome, SubagentConfig, SubagentHandle, run_session, spawn_subagent,
};
pub use error::AcpError;
pub use fs::AcpFileExecutor;
pub use lsp::{AcpLspProvider, DiagnosticsCache, LspProvider};
pub use mcp_bridge::acp_mcp_servers_to_entries;
pub use permission::AcpPermissionGate;
pub use terminal::AcpShellExecutor;
pub use transport::{
    AcpClientToken, AcpServerConfig, OWNER_KEY_DEFAULT, OWNER_KEY_LOCAL, serve_connection,
    serve_stdio,
};

#[cfg(feature = "acp-http")]
pub use agent::SendAgentSpawner;
#[cfg(feature = "acp-http")]
pub use transport::{AcpHttpState, acp_router};

// Crate-major-version bumps of `agent-client-protocol` must never silently change the ACP
// *wire* protocol version Zeph advertises. With `unstable_protocol_v2` off (never forwarded by
// Zeph — see the `agent-client-protocol-schema` feature list), `LATEST` is hardcoded to `V1` by
// the pinned schema crate, so this assertion is a tautology for the version pinned today; its
// value is as a regression guard against a *future* schema-pin bump that redefines `LATEST` to
// something other than `1` — that failure mode would otherwise only surface as a silent wire
// behavior change, not a compile error.
const _: () = assert!(
    agent_client_protocol::schema::ProtocolVersion::LATEST.as_u16() == 1,
    "ACP wire protocol version must stay pinned at 1 across agent-client-protocol crate bumps"
);

/// Wire protocol type for an LLM provider, used to populate [`AcpServerConfig::provider_names`].
#[cfg(feature = "unstable-llm-providers")]
pub use agent_client_protocol_schema::v1::LlmProtocol;

#[cfg(test)]
mod tests {
    // Deliberately hardcodes the literal `1` rather than deriving it from `ProtocolVersion::LATEST`
    // (unlike the `const _` guard above and `discovery_returns_expected_json_fields`, which both
    // compare against the live symbol and would therefore stay green even if `LATEST` were
    // silently redefined). This is the one check in the suite that fails if someone weakens the
    // wire-version invariant itself.
    #[test]
    fn protocol_version_latest_is_hardcoded_wire_v1() {
        assert_eq!(
            agent_client_protocol::schema::ProtocolVersion::LATEST.as_u16(),
            1
        );
    }
}
