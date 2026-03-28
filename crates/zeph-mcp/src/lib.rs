// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MCP client lifecycle, tool discovery, and execution.

pub mod attestation;
pub mod caller;
pub mod client;
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
pub use embedding_guard::{EmbeddingAnomalyGuard, EmbeddingGuardEvent, EmbeddingGuardResult};
pub use error::McpError;
pub use executor::McpToolExecutor;
pub use manager::{McpManager, McpTransport, McpTrustLevel, ServerConnectOutcome, ServerEntry};
#[cfg(feature = "mock")]
pub use mock::{McpCall, MockMcpCaller};
pub use policy::{McpPolicy, PolicyEnforcer, PolicyViolation, RateLimit};
pub use prober::{DefaultMcpProber, ProbeResult};
pub use prompt::format_mcp_tools_prompt;
pub use pruning::{
    PruningCache, PruningError, PruningParams, content_hash, prune_tools, prune_tools_cached,
    tool_list_hash,
};
pub use registry::McpToolRegistry;
pub use semantic_index::{
    DiscoveryParams, SemanticIndexError, SemanticToolIndex, ToolDiscoveryStrategy,
};
pub use tool::McpTool;
pub use trust_score::{ServerTrustScore, TrustScoreStore};
