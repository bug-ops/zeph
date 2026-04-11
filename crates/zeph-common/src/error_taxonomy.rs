// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared error classification enums for tool invocation failures.
//!
//! Only the pure data types ([`ToolErrorCategory`] and [`ErrorDomain`]) live here.
//! The `classify_*` helper functions and executor-specific types remain in `zeph-tools`,
//! which may depend on `std::io::Error` and HTTP status codes.

/// High-level error domain for recovery strategy dispatch.
///
/// Groups the `ToolErrorCategory` variants into 4 domains that map to distinct
/// recovery strategies in the agent loop. Does NOT replace `ToolErrorCategory` — it
/// is a companion abstraction for coarse dispatch.
///
/// # Examples
///
/// ```rust
/// use zeph_common::error_taxonomy::ErrorDomain;
///
/// assert!(ErrorDomain::System.is_auto_retryable());
/// assert!(!ErrorDomain::Planning.is_auto_retryable());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorDomain {
    /// The agent selected the wrong tool or misunderstood the task.
    /// Recovery: re-plan, pick a different tool or approach.
    /// Categories: `ToolNotFound`
    Planning,

    /// The agent's output (parameters, types) was malformed.
    /// Recovery: reformat parameters using tool schema, retry once.
    /// Categories: `InvalidParameters`, `TypeMismatch`
    Reflection,

    /// External action failed due to policy or resource constraints.
    /// Recovery: inform user, suggest alternative, or skip.
    /// Categories: `PolicyBlocked`, `ConfirmationRequired`, `PermanentFailure`, `Cancelled`
    Action,

    /// Transient infrastructure failure.
    /// Recovery: automatic retry with backoff.
    /// Categories: `RateLimited`, `ServerError`, `NetworkError`, `Timeout`
    System,
}

impl ErrorDomain {
    /// Whether errors in this domain should trigger automatic retry.
    #[must_use]
    pub fn is_auto_retryable(self) -> bool {
        matches!(self, Self::System)
    }

    /// Whether the LLM should be asked to fix its output.
    #[must_use]
    pub fn needs_llm_correction(self) -> bool {
        matches!(self, Self::Reflection | Self::Planning)
    }

    /// Human-readable label for audit logs.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::Reflection => "reflection",
            Self::Action => "action",
            Self::System => "system",
        }
    }
}

/// Invocation phase in which a tool failure occurred, per arXiv:2601.16280.
///
/// Maps Zeph's `ToolErrorCategory` variants to the 4-phase diagnostic framework:
/// Setup → `ParamHandling` → Execution → `ResultInterpretation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInvocationPhase {
    /// Tool lookup/registration phase: was the tool name valid?
    Setup,
    /// Parameter validation phase: were the provided arguments well-formed?
    ParamHandling,
    /// Runtime execution phase: did the tool run successfully?
    Execution,
    /// Output parsing/interpretation phase: was the result usable?
    /// Reserved for future use — no current `ToolErrorCategory` maps here.
    ResultInterpretation,
}

impl ToolInvocationPhase {
    /// Human-readable label for audit logs.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::ParamHandling => "param_handling",
            Self::Execution => "execution",
            Self::ResultInterpretation => "result_interpretation",
        }
    }
}

/// Fine-grained 12-category classification of tool invocation errors.
///
/// Each category determines retry eligibility, LLM parameter reformat path,
/// quality attribution for reputation scoring, and structured feedback content.
///
/// # Examples
///
/// ```rust
/// use zeph_common::error_taxonomy::ToolErrorCategory;
///
/// assert!(ToolErrorCategory::RateLimited.is_retryable());
/// assert!(!ToolErrorCategory::InvalidParameters.is_retryable());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub enum ToolErrorCategory {
    // ── Initialization failures ──────────────────────────────────────────
    /// Tool name not found in the registry (LLM requested a non-existent tool).
    ToolNotFound,

    // ── Parameter failures ───────────────────────────────────────────────
    /// LLM provided invalid or missing parameters for the tool.
    InvalidParameters,
    /// Parameter type mismatch (e.g., string where integer expected).
    TypeMismatch,

    // ── Permission / policy failures ─────────────────────────────────────
    /// Blocked by security policy (blocklist, sandbox, trust gate).
    PolicyBlocked,
    /// Requires user confirmation before execution.
    ConfirmationRequired,

    // ── Execution failures (permanent) ───────────────────────────────────
    /// HTTP 403/404 or equivalent permanent resource rejection.
    PermanentFailure,
    /// Operation cancelled by the user.
    Cancelled,

    // ── Execution failures (transient) ───────────────────────────────────
    /// HTTP 429 (rate limit) or resource exhaustion.
    RateLimited,
    /// HTTP 5xx or equivalent server-side error.
    ServerError,
    /// Network connectivity failure (DNS, connection refused, reset).
    NetworkError,
    /// Operation timed out.
    Timeout,
}

impl ToolErrorCategory {
    /// Whether this error category is eligible for automatic retry with backoff.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimited | Self::ServerError | Self::NetworkError | Self::Timeout
        )
    }

    /// Whether the LLM should be asked to reformat parameters and retry.
    ///
    /// Only `InvalidParameters` and `TypeMismatch` trigger the reformat path.
    #[must_use]
    pub fn needs_parameter_reformat(self) -> bool {
        matches!(self, Self::InvalidParameters | Self::TypeMismatch)
    }

    /// Whether this error is attributable to LLM output quality.
    ///
    /// Infrastructure errors (network, timeout, server, rate limit) are NOT
    /// the model's fault and must never trigger self-reflection.
    #[must_use]
    pub fn is_quality_failure(self) -> bool {
        matches!(
            self,
            Self::InvalidParameters | Self::TypeMismatch | Self::ToolNotFound
        )
    }

    /// Map to the high-level error domain for recovery dispatch.
    #[must_use]
    pub fn domain(self) -> ErrorDomain {
        match self {
            Self::ToolNotFound => ErrorDomain::Planning,
            Self::InvalidParameters | Self::TypeMismatch => ErrorDomain::Reflection,
            Self::PolicyBlocked
            | Self::ConfirmationRequired
            | Self::PermanentFailure
            | Self::Cancelled => ErrorDomain::Action,
            Self::RateLimited | Self::ServerError | Self::NetworkError | Self::Timeout => {
                ErrorDomain::System
            }
        }
    }

    /// Human-readable label for audit logs, TUI status indicators, and structured feedback.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ToolNotFound => "tool_not_found",
            Self::InvalidParameters => "invalid_parameters",
            Self::TypeMismatch => "type_mismatch",
            Self::PolicyBlocked => "policy_blocked",
            Self::ConfirmationRequired => "confirmation_required",
            Self::PermanentFailure => "permanent_failure",
            Self::Cancelled => "cancelled",
            Self::RateLimited => "rate_limited",
            Self::ServerError => "server_error",
            Self::NetworkError => "network_error",
            Self::Timeout => "timeout",
        }
    }

    /// Map to the diagnostic invocation phase per arXiv:2601.16280.
    #[must_use]
    pub fn phase(self) -> ToolInvocationPhase {
        match self {
            Self::ToolNotFound => ToolInvocationPhase::Setup,
            Self::InvalidParameters | Self::TypeMismatch => ToolInvocationPhase::ParamHandling,
            Self::PolicyBlocked
            | Self::ConfirmationRequired
            | Self::PermanentFailure
            | Self::Cancelled
            | Self::RateLimited
            | Self::ServerError
            | Self::NetworkError
            | Self::Timeout => ToolInvocationPhase::Execution,
        }
    }

    /// Recovery suggestion for the LLM based on error category.
    #[must_use]
    pub fn suggestion(self) -> &'static str {
        match self {
            Self::ToolNotFound => {
                "Check the tool name. Use tool_definitions to see available tools."
            }
            Self::InvalidParameters => "Review the tool schema and provide correct parameters.",
            Self::TypeMismatch => "Check parameter types against the tool schema.",
            Self::PolicyBlocked => {
                "This operation is blocked by security policy. Try an alternative approach."
            }
            Self::ConfirmationRequired => "This operation requires user confirmation.",
            Self::PermanentFailure => {
                "This resource is not available. Try an alternative approach."
            }
            Self::Cancelled => "Operation was cancelled by the user.",
            Self::RateLimited => "Rate limit exceeded. The system will retry if possible.",
            Self::ServerError => "Server error. The system will retry if possible.",
            Self::NetworkError => "Network error. The system will retry if possible.",
            Self::Timeout => "Operation timed out. The system will retry if possible.",
        }
    }
}
