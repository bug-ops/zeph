// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MCP client lifecycle, multi-server management, and Qdrant tool registry.
//!
//! `zeph-mcp` implements the [Model Context Protocol](https://spec.modelcontextprotocol.io/)
//! client for Zeph. It manages connections to multiple MCP servers simultaneously,
//! discovers and registers their tools, and executes tool calls with a layered security
//! pipeline.
//!
//! # Architecture
//!
//! ```text
//! McpManager ──► McpClient (one per server, via rmcp)
//!     │              └── ToolListChangedHandler (refresh notifications)
//!     │
//!     ├── PolicyEnforcer   (allowlists, denylists, rate limiting)
//!     ├── DefaultMcpProber (pre-connect resource/prompt injection scan)
//!     ├── TrustScoreStore  (per-server persistent score with decay)
//!     ├── EmbeddingAnomalyGuard (post-call drift detection)
//!     └── sanitize_tools() (always-on prompt injection scrubbing)
//!
//! McpToolExecutor ──► McpManager::call_tool()
//!     implements ToolExecutor for zeph-tools dispatch
//!
//! McpToolRegistry ──► Qdrant (vector search for tool discovery)
//! SemanticToolIndex ──► in-memory cosine similarity (fast, no Qdrant)
//! ```
//!
//! # Transport types
//!
//! Servers are connected via three transport types (see [`McpTransport`]):
//!
//! - **Stdio** — spawns a child process; suitable for local MCP servers (e.g. `npx`, `uvx`).
//! - **Http** — streamable HTTP with optional static headers.
//! - **OAuth** — OAuth 2.1 authenticated HTTP; performs a browser-based authorization flow.
//!
//! # Security pipeline
//!
//! Every tool definition goes through the following checks before reaching the agent:
//!
//! 1. **Command allowlist** (`security.rs`) — Stdio server commands must be on the allowlist.
//! 2. **SSRF validation** — HTTP URLs are resolved and blocked if they point to private/reserved IPs.
//! 3. **Pre-connect probing** (`prober.rs`) — scans `resources/list` and `prompts/list` for
//!    injection patterns; updates the persistent trust score.
//! 4. **Attestation** (`attestation.rs`) — compares the actual tool set against the operator's
//!    `expected_tools` list and detects schema drift between connections.
//! 5. **Sanitization** (`sanitize.rs`) — scrubs tool `name`, `description`, and `input_schema`
//!    for prompt injection patterns; always on, cannot be disabled.
//! 6. **Data-flow policy** (`policy.rs`) — blocks high-sensitivity tools on untrusted servers.
//! 7. **Embedding anomaly guard** (`embedding_guard.rs`) — post-call drift detection.
//!
//! # Trust levels
//!
//! Each server is assigned a [`McpTrustLevel`]:
//!
//! - `Trusted` — SSRF skip, all tools exposed (operator-controlled servers).
//! - `Untrusted` (default) — SSRF enforced; tools shown with a warning when allowlist is absent.
//! - `Sandboxed` — strict mode; only allowlisted tools exposed; elicitation disabled.
//!
//! # Examples
//!
//! Connect to an stdio MCP server and call a tool:
//!
//! ```no_run
//! use std::collections::HashMap;
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! use zeph_mcp::{McpManager, McpTransport, ServerEntry, McpTrustLevel};
//! use zeph_mcp::policy::PolicyEnforcer;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let server = ServerEntry {
//!     id: "filesystem".to_owned(),
//!     transport: McpTransport::Stdio {
//!         command: "npx".to_owned(),
//!         args: vec!["-y".to_owned(), "@modelcontextprotocol/server-filesystem".to_owned()],
//!         env: HashMap::new(),
//!     },
//!     timeout: Duration::from_secs(30),
//!     trust_level: McpTrustLevel::Untrusted,
//!     tool_allowlist: None,
//!     expected_tools: vec![],
//!     roots: vec![],
//!     tool_metadata: HashMap::new(),
//!     elicitation_enabled: false,
//!     elicitation_timeout_secs: 120,
//!     env_isolation: false,
//! };
//!
//! let manager = McpManager::new(
//!     vec![server],
//!     vec!["npx".to_owned()],
//!     PolicyEnforcer::new(vec![]),
//! );
//!
//! let (tools, _outcomes) = manager.connect_all().await;
//! println!("Connected {} tools", tools.len());
//! # Ok(())
//! # }
//! ```

#[allow(unused_imports)]
pub(crate) use zeph_db::sql;

pub mod attestation;
pub mod caller;
pub mod client;
pub mod elicitation;
pub mod embedding_guard;
pub mod error;
pub mod executor;
pub mod manager;
pub mod oauth;
pub mod policy;
pub mod prober;
pub mod prompt;
pub mod pruning;
pub mod registry;
pub mod sanitize;
pub mod security;
pub mod semantic_index;
pub mod tool;
pub mod trust_score;

#[cfg(test)]
pub mod testing;

#[cfg(feature = "mock")]
pub mod mock;

pub use attestation::{AttestationResult, ServerTrustBoundary, ToolFingerprint, attest_tools};
pub use caller::McpCaller;
pub use client::{OAuthConnectResult, OAuthPending, ToolRefreshEvent};
pub use elicitation::ElicitationEvent;
pub use embedding_guard::{EmbeddingAnomalyGuard, EmbeddingGuardEvent, EmbeddingGuardResult};
pub use error::{McpError, McpErrorCode};
pub use executor::McpToolExecutor;
pub use manager::{McpManager, McpTransport, McpTrustLevel, ServerConnectOutcome, ServerEntry};
#[cfg(feature = "mock")]
pub use mock::{McpCall, MockMcpCaller};
pub use policy::{
    DataFlowViolation, McpPolicy, PolicyEnforcer, PolicyViolation, RateLimit, check_data_flow,
};
pub use prober::{DefaultMcpProber, ProbeResult};
pub use prompt::format_mcp_tools_prompt;
pub use pruning::{
    PruningCache, PruningError, PruningParams, content_hash, prune_tools, prune_tools_cached,
    tool_list_hash,
};
pub use registry::McpToolRegistry;
pub use sanitize::SanitizeResult;
pub use semantic_index::{
    DiscoveryParams, SemanticIndexError, SemanticToolIndex, ToolDiscoveryStrategy,
};
pub use tool::{CapabilityClass, DataSensitivity, McpTool, ToolSecurityMeta, infer_security_meta};
pub use trust_score::{ServerTrustScore, TrustScoreStore};
