// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool execution abstraction, shell backend, web scraping, and audit logging for Zeph.
//!
//! This crate provides the [`ToolExecutor`] trait and its concrete implementations:
//!
//! - [`ShellExecutor`] — executes bash blocks from LLM responses with sandboxing, blocklists,
//!   output filtering, transactional rollback, and audit logging.
//! - [`WebScrapeExecutor`] — fetches and scrapes web pages via CSS selectors, with SSRF
//!   protection and domain policies.
//! - [`CompositeExecutor`] — chains two executors with first-match-wins dispatch.
//! - [`FileExecutor`] — reads and writes local files within a sandbox.
//! - [`DiagnosticsExecutor`] — exposes agent self-diagnostics as a tool.
//!
//! # Architecture
//!
//! The primary abstraction is [`ToolExecutor`], an async trait implemented by every backend.
//! When dynamic dispatch is needed (e.g., storing heterogeneous executors in a `Vec`), use
//! [`ErasedToolExecutor`] or wrap with [`DynExecutor`].
//!
//! Tool calls originate from two paths:
//!
//! 1. **Fenced code blocks** — legacy LLM responses containing ` ```bash ` or ` ```scrape `
//!    blocks dispatched via [`ToolExecutor::execute`].
//! 2. **Structured tool calls** — modern JSON tool calls dispatched via
//!    [`ToolExecutor::execute_tool_call`].
//!
//! # Security
//!
//! Every executor enforces security controls before execution:
//!
//! - [`ShellExecutor`] checks the command against a blocklist, validates paths against an
//!   allowlist sandbox, and optionally requires user confirmation for destructive patterns.
//! - [`WebScrapeExecutor`] validates the URL scheme (HTTPS only), resolves DNS, and rejects
//!   private-network addresses (SSRF protection).
//! - [`AuditLogger`] writes a structured JSONL entry for every tool invocation.
//!
//! # Example
//!
//! ```rust,no_run
//! use zeph_tools::{ShellExecutor, ToolExecutor, ShellConfig};
//!
//! # async fn example() {
//! let config = ShellConfig::default();
//! let executor = ShellExecutor::new(&config);
//!
//! // Execute a fenced bash block from an LLM response.
//! let response = "```bash\necho hello\n```";
//! if let Ok(Some(output)) = executor.execute(response).await {
//!     println!("{}", output.summary);
//! }
//! # }
//! ```

// TODO(critic): post-v1.0 — re-evaluate splitting executor / web / shell into sub-crates if compile times degrade.

pub mod adversarial_gate;
pub mod adversarial_policy;
pub mod anomaly;
pub mod audit;
pub mod cache;
pub mod composite;
pub mod config;
pub mod cwd;
pub mod diagnostics;
pub mod domain_match;
pub mod error_taxonomy;
pub mod executor;
pub mod file;
pub mod filter;
pub mod net;
pub mod patterns;
pub mod permissions;
pub mod policy;
pub mod policy_gate;
pub mod registry;
pub mod sandbox;
pub mod schema_filter;
pub mod scope;
pub mod scrape;
pub mod search_code;
pub mod shell;
pub mod tool_filter;
pub mod trust_gate;
pub mod trust_level;
pub mod utility;
pub mod verifier;
pub use adversarial_gate::AdversarialPolicyGateExecutor;
pub use adversarial_policy::{
    PolicyDecision as AdversarialPolicyDecision, PolicyLlmClient, PolicyMessage, PolicyRole,
    PolicyValidator, parse_policy_lines,
};
pub use anomaly::{AnomalyDetector, AnomalySeverity, is_reasoning_model};
pub use audit::{
    AuditEntry, AuditLogger, AuditResult, EgressEvent, VigilRiskLevel, chrono_now,
    log_tool_risk_summary,
};
pub use cache::{CacheKey, ToolResultCache, is_cacheable};
pub use composite::CompositeExecutor;
pub use config::{build_permission_policy, validate_sandbox_denied_domains};
pub use cwd::SetCwdExecutor;
pub use diagnostics::DiagnosticsExecutor;
pub use error_taxonomy::{
    ErrorDomain, ToolErrorCategory, ToolErrorFeedback, ToolInvocationPhase, classify_http_status,
    classify_io_error,
};
pub use executor::{
    ClaimSource, DiffData, DynExecutor, ErasedToolExecutor, ErrorKind, FilterStats,
    MAX_TOOL_OUTPUT_CHARS, TOOL_EVENT_CHANNEL_CAP, ToolCall, ToolError, ToolEvent, ToolEventRx,
    ToolEventTx, ToolExecutor, ToolOutput, truncate_tool_output, truncate_tool_output_at,
};
pub use file::FileExecutor;
pub use filter::{
    CommandMatcher, FilterConfidence, FilterMetrics, FilterResult, OutputFilter,
    OutputFilterRegistry, sanitize_output, strip_ansi,
};
pub use net::is_private_ip;
pub use permissions::PermissionPolicy;
pub use policy::{PolicyCompileError, PolicyContext, PolicyDecision, PolicyEnforcer};
pub use policy_gate::{PolicyGateExecutor, RiskSignalQueue, TrajectoryRiskSlot};
pub use registry::ToolRegistry;
#[cfg(target_os = "macos")]
pub use sandbox::MacosSandbox;
pub use sandbox::{
    NoopSandbox, Sandbox, SandboxError, SandboxPolicy, build_sandbox, build_sandbox_with_policy,
};
pub use schema_filter::{
    DependencyExclusion, InclusionReason, ToolDependencyGraph, ToolEmbedding, ToolFilterResult,
    ToolSchemaFilter,
};
pub use scope::{ScopeError, ScopeWarning, ScopedToolExecutor, ToolScope, build_scoped_executor};
pub use scrape::WebScrapeExecutor;
pub use search_code::{
    LspSearchBackend, SearchCodeExecutor, SearchCodeHit, SearchCodeSource, SemanticSearchBackend,
};
pub use shell::background::{BackgroundCompletion, BackgroundRunSnapshot, RunId};
pub use shell::{
    DEFAULT_BLOCKED_COMMANDS, SHELL_INTERPRETERS, ShellExecutor, ShellOutputEnvelope,
    ShellPolicyHandle, check_blocklist, effective_shell_command,
};
pub use tool_filter::ToolFilter;
pub use trust_gate::TrustGateExecutor;
pub use trust_level::SkillTrustLevel;
pub use utility::{
    UtilityAction, UtilityContext, UtilityScore, UtilityScorer, has_explicit_tool_request,
};
pub use verifier::{
    DestructiveCommandVerifier, FirewallVerifier, InjectionPatternVerifier, PreExecutionVerifier,
    UrlGroundingVerifier, VerificationResult,
};
pub use zeph_common::ToolName;
pub use zeph_config::tools::{
    AdversarialPolicyConfig, AnomalyConfig, AuditConfig, AuthorizationConfig, DefaultEffect,
    DependencyConfig, EgressConfig, FileConfig, FilterConfig, OverflowConfig, PolicyConfig,
    PolicyEffect, PolicyRuleConfig, ResultCacheConfig, RetryConfig, SandboxConfig, SandboxProfile,
    ScrapeConfig, SecurityFilterConfig, ShellConfig, TafcConfig, ToolDependency, ToolsConfig,
    UtilityScoringConfig,
};
pub use zeph_config::tools::{
    AutonomyLevel, PermissionAction, PermissionRule, PermissionsConfig, SpeculationMode,
    SpeculativeAllowlistConfig, SpeculativeConfig, SpeculativePatternConfig,
};
pub use zeph_config::tools::{
    DestructiveVerifierConfig, FirewallVerifierConfig, InjectionVerifierConfig,
    PreExecutionVerifierConfig, UrlGroundingVerifierConfig,
};
