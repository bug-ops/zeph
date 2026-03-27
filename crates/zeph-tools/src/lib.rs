// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool execution abstraction and shell backend.

pub mod anomaly;
pub mod audit;
pub mod cache;
pub mod composite;
pub mod config;
pub mod diagnostics;
pub mod executor;
pub mod file;
pub mod filter;
pub mod net;
pub mod patterns;
pub mod permissions;
#[cfg(feature = "policy-enforcer")]
pub mod policy;
#[cfg(feature = "policy-enforcer")]
pub mod policy_gate;
pub mod registry;
pub mod schema_filter;
pub mod scrape;
pub mod search_code;
pub mod shell;
pub mod tool_filter;
pub mod trust_gate;
pub mod trust_level;
pub mod verifier;

pub use anomaly::{AnomalyDetector, AnomalySeverity};
pub use audit::{AuditEntry, AuditLogger, AuditResult};
pub use cache::{CacheKey, ToolResultCache, is_cacheable};
pub use composite::CompositeExecutor;
pub use config::{
    AnomalyConfig, AuditConfig, DependencyConfig, OverflowConfig, ResultCacheConfig, ScrapeConfig,
    ShellConfig, TafcConfig, ToolDependency, ToolsConfig,
};
pub use diagnostics::DiagnosticsExecutor;
pub use executor::{
    DiffData, DynExecutor, ErasedToolExecutor, ErrorKind, FilterStats, MAX_TOOL_OUTPUT_CHARS,
    ToolCall, ToolError, ToolEvent, ToolEventTx, ToolExecutor, ToolOutput, truncate_tool_output,
    truncate_tool_output_at,
};
pub use file::FileExecutor;
pub use filter::{
    CommandMatcher, FilterConfidence, FilterConfig, FilterMetrics, FilterResult, OutputFilter,
    OutputFilterRegistry, sanitize_output, strip_ansi,
};
pub use net::is_private_ip;
pub use permissions::{
    AutonomyLevel, PermissionAction, PermissionPolicy, PermissionRule, PermissionsConfig,
};
#[cfg(feature = "policy-enforcer")]
pub use policy::{
    DefaultEffect, PolicyCompileError, PolicyConfig, PolicyContext, PolicyDecision, PolicyEffect,
    PolicyEnforcer, PolicyRuleConfig,
};
#[cfg(feature = "policy-enforcer")]
pub use policy_gate::PolicyGateExecutor;
pub use registry::ToolRegistry;
pub use schema_filter::{
    DependencyExclusion, InclusionReason, ToolDependencyGraph, ToolEmbedding, ToolFilterResult,
    ToolSchemaFilter,
};
pub use scrape::WebScrapeExecutor;
pub use search_code::{
    LspSearchBackend, SearchCodeExecutor, SearchCodeHit, SearchCodeSource, SemanticSearchBackend,
};
pub use shell::{
    DEFAULT_BLOCKED_COMMANDS, SHELL_INTERPRETERS, ShellExecutor, check_blocklist,
    effective_shell_command,
};
pub use tool_filter::ToolFilter;
pub use trust_gate::TrustGateExecutor;
pub use trust_level::TrustLevel;
pub use verifier::{
    DestructiveCommandVerifier, DestructiveVerifierConfig, InjectionPatternVerifier,
    InjectionVerifierConfig, PreExecutionVerifier, PreExecutionVerifierConfig,
    UrlGroundingVerifier, UrlGroundingVerifierConfig, VerificationResult,
};
