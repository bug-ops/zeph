// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Security event category shared across Zeph crates.
//!
//! Moved from `zeph-core::metrics` so that `zeph-agent-context` can define a
//! `SecurityEventSink` trait without depending on `zeph-core`.

/// Category of a security event used for TUI display and audit logging.
///
/// Each variant maps to a short string key via [`SecurityEventCategory::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityEventCategory {
    /// Prompt-injection flag raised by the sanitizer.
    InjectionFlag,
    /// ML classifier hard-blocked tool output (`enforcement_mode=block` only).
    InjectionBlocked,
    /// Potential data exfiltration blocked by the sanitizer.
    ExfiltrationBlock,
    /// Content quarantined for human review.
    Quarantine,
    /// Output truncated due to length or injection risk.
    Truncation,
    /// Request rate-limited.
    RateLimit,
    /// Memory write validation rejected the content.
    MemoryValidation,
    /// Tool call blocked before execution.
    PreExecutionBlock,
    /// Tool call flagged as suspicious before execution.
    PreExecutionWarn,
    /// LLM response failed post-generation verification.
    ResponseVerification,
    /// `TurnCausalAnalyzer` flagged behavioral deviation at tool-return boundary.
    CausalIpiFlag,
    /// MCP tool result crossing into an ACP-serving session boundary.
    CrossBoundaryMcpToAcp,
    /// VIGIL pre-sanitizer gate flagged a tool output.
    VigilFlag,
}

impl SecurityEventCategory {
    /// Returns a short ASCII string key for this category.
    ///
    /// Used as the `category` column in audit logs and TUI display.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InjectionFlag => "injection",
            Self::InjectionBlocked => "injection_blocked",
            Self::ExfiltrationBlock => "exfil",
            Self::Quarantine => "quarantine",
            Self::Truncation => "truncation",
            Self::RateLimit => "rate_limit",
            Self::MemoryValidation => "memory_validation",
            Self::PreExecutionBlock => "pre_exec_block",
            Self::PreExecutionWarn => "pre_exec_warn",
            Self::ResponseVerification => "response_verify",
            Self::CausalIpiFlag => "causal_ipi",
            Self::CrossBoundaryMcpToAcp => "cross_boundary_mcp_to_acp",
            Self::VigilFlag => "vigil",
        }
    }
}
