// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! 12-category tool invocation error taxonomy (arXiv:2601.16280).
//!
//! Provides fine-grained error classification beyond the binary `ErrorKind`
//! (Transient/Permanent), enabling category-specific recovery strategies,
//! structured LLM feedback, and quality-attributable reputation scoring.

use crate::executor::ErrorKind;

/// Fine-grained 12-category classification of tool invocation errors.
///
/// Each category determines retry eligibility, LLM parameter reformat path,
/// quality attribution for reputation scoring, and structured feedback content.
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
    /// A single reformat attempt is allowed; if it fails, the error is final.
    #[must_use]
    pub fn needs_parameter_reformat(self) -> bool {
        matches!(self, Self::InvalidParameters | Self::TypeMismatch)
    }

    /// Whether this error is attributable to LLM output quality.
    ///
    /// Quality failures affect reputation scoring in triage routing and are the
    /// only category for which `attempt_self_reflection` should be triggered.
    /// Infrastructure errors (network, timeout, server, rate limit) are NOT
    /// the model's fault and must never trigger self-reflection.
    #[must_use]
    pub fn is_quality_failure(self) -> bool {
        matches!(
            self,
            Self::InvalidParameters | Self::TypeMismatch | Self::ToolNotFound
        )
    }

    /// Coarse classification for backward compatibility with existing `ErrorKind`.
    #[must_use]
    pub fn error_kind(self) -> ErrorKind {
        if self.is_retryable() {
            ErrorKind::Transient
        } else {
            ErrorKind::Permanent
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
            Self::RateLimited => "Rate limit exceeded. The system will retry automatically.",
            Self::ServerError => "Server error. The system will retry automatically.",
            Self::NetworkError => "Network error. The system will retry automatically.",
            Self::Timeout => "Operation timed out. The system will retry automatically.",
        }
    }
}

/// Structured error feedback injected as `tool_result` content for classified errors.
///
/// Provides the LLM with actionable information about what went wrong and what to
/// do next, replacing the opaque `[error] ...` string format.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolErrorFeedback {
    pub category: ToolErrorCategory,
    pub message: String,
    pub retryable: bool,
}

impl ToolErrorFeedback {
    /// Format as a structured string for injection into `tool_result` content.
    #[must_use]
    pub fn format_for_llm(&self) -> String {
        format!(
            "[tool_error]\ncategory: {}\nerror: {}\nsuggestion: {}\nretryable: {}",
            self.category.label(),
            self.message,
            self.category.suggestion(),
            self.retryable,
        )
    }
}

/// Classify an HTTP status code into a `ToolErrorCategory`.
#[must_use]
pub fn classify_http_status(status: u16) -> ToolErrorCategory {
    match status {
        400 | 422 => ToolErrorCategory::InvalidParameters,
        401 | 403 => ToolErrorCategory::PolicyBlocked,
        429 => ToolErrorCategory::RateLimited,
        500..=599 => ToolErrorCategory::ServerError,
        // 404, 410, and all other non-success codes: permanent failure.
        _ => ToolErrorCategory::PermanentFailure,
    }
}

/// Classify an `io::Error` into a `ToolErrorCategory`.
///
/// # Note on `io::ErrorKind::NotFound`
///
/// `NotFound` from an `Execution` error means a file or binary was not found at the
/// OS level (e.g., `bash: command not found`). This is NOT the same as "tool not found
/// in registry" (`ToolNotFound`). We map it to `PermanentFailure` to avoid incorrectly
/// penalizing the model for OS-level path issues.
#[must_use]
pub fn classify_io_error(err: &std::io::Error) -> ToolErrorCategory {
    match err.kind() {
        std::io::ErrorKind::TimedOut => ToolErrorCategory::Timeout,
        std::io::ErrorKind::ConnectionRefused
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::BrokenPipe => ToolErrorCategory::NetworkError,
        // WouldBlock / Interrupted are async runtime signals, not true network failures,
        // but they are transient and retryable — map to ServerError as the generic
        // retryable catch-all rather than NetworkError to avoid misleading audit labels.
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted => {
            ToolErrorCategory::ServerError
        }
        std::io::ErrorKind::PermissionDenied => ToolErrorCategory::PolicyBlocked,
        // OS-level file/binary not found is a permanent execution failure, not a registry miss.
        // ToolNotFound is reserved for registry misses (LLM requested an unknown tool name).
        _ => ToolErrorCategory::PermanentFailure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_categories() {
        assert!(ToolErrorCategory::RateLimited.is_retryable());
        assert!(ToolErrorCategory::ServerError.is_retryable());
        assert!(ToolErrorCategory::NetworkError.is_retryable());
        assert!(ToolErrorCategory::Timeout.is_retryable());

        assert!(!ToolErrorCategory::InvalidParameters.is_retryable());
        assert!(!ToolErrorCategory::TypeMismatch.is_retryable());
        assert!(!ToolErrorCategory::ToolNotFound.is_retryable());
        assert!(!ToolErrorCategory::PolicyBlocked.is_retryable());
        assert!(!ToolErrorCategory::PermanentFailure.is_retryable());
        assert!(!ToolErrorCategory::Cancelled.is_retryable());
        assert!(!ToolErrorCategory::ConfirmationRequired.is_retryable());
    }

    #[test]
    fn quality_failure_categories() {
        assert!(ToolErrorCategory::InvalidParameters.is_quality_failure());
        assert!(ToolErrorCategory::TypeMismatch.is_quality_failure());
        assert!(ToolErrorCategory::ToolNotFound.is_quality_failure());

        // Infrastructure errors must NOT be quality failures — they must not trigger
        // self-reflection, as they are not attributable to LLM output quality.
        assert!(!ToolErrorCategory::NetworkError.is_quality_failure());
        assert!(!ToolErrorCategory::ServerError.is_quality_failure());
        assert!(!ToolErrorCategory::RateLimited.is_quality_failure());
        assert!(!ToolErrorCategory::Timeout.is_quality_failure());
        assert!(!ToolErrorCategory::PolicyBlocked.is_quality_failure());
        assert!(!ToolErrorCategory::PermanentFailure.is_quality_failure());
        assert!(!ToolErrorCategory::Cancelled.is_quality_failure());
    }

    #[test]
    fn needs_parameter_reformat() {
        assert!(ToolErrorCategory::InvalidParameters.needs_parameter_reformat());
        assert!(ToolErrorCategory::TypeMismatch.needs_parameter_reformat());
        assert!(!ToolErrorCategory::NetworkError.needs_parameter_reformat());
        assert!(!ToolErrorCategory::ToolNotFound.needs_parameter_reformat());
    }

    #[test]
    fn error_kind_backward_compat() {
        // Retryable categories → Transient
        assert_eq!(
            ToolErrorCategory::NetworkError.error_kind(),
            ErrorKind::Transient
        );
        assert_eq!(
            ToolErrorCategory::Timeout.error_kind(),
            ErrorKind::Transient
        );
        // Non-retryable → Permanent
        assert_eq!(
            ToolErrorCategory::InvalidParameters.error_kind(),
            ErrorKind::Permanent
        );
        assert_eq!(
            ToolErrorCategory::PolicyBlocked.error_kind(),
            ErrorKind::Permanent
        );
    }

    #[test]
    fn classify_http_status_codes() {
        assert_eq!(classify_http_status(403), ToolErrorCategory::PolicyBlocked);
        assert_eq!(
            classify_http_status(404),
            ToolErrorCategory::PermanentFailure
        );
        assert_eq!(
            classify_http_status(422),
            ToolErrorCategory::InvalidParameters
        );
        assert_eq!(classify_http_status(429), ToolErrorCategory::RateLimited);
        assert_eq!(classify_http_status(500), ToolErrorCategory::ServerError);
        assert_eq!(classify_http_status(503), ToolErrorCategory::ServerError);
        assert_eq!(
            classify_http_status(200),
            ToolErrorCategory::PermanentFailure
        );
    }

    #[test]
    fn classify_io_not_found_is_permanent_not_tool_not_found() {
        // B2 fix: OS-level NotFound must NOT map to ToolNotFound.
        // ToolNotFound is reserved for registry misses (LLM requested unknown tool name).
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "No such file or directory");
        assert_eq!(classify_io_error(&err), ToolErrorCategory::PermanentFailure);
    }

    #[test]
    fn classify_io_connection_errors() {
        let refused =
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        assert_eq!(classify_io_error(&refused), ToolErrorCategory::NetworkError);

        let reset = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        assert_eq!(classify_io_error(&reset), ToolErrorCategory::NetworkError);

        let timed_out = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        assert_eq!(classify_io_error(&timed_out), ToolErrorCategory::Timeout);
    }

    #[test]
    fn tool_error_feedback_format() {
        let fb = ToolErrorFeedback {
            category: ToolErrorCategory::InvalidParameters,
            message: "missing required field: url".to_owned(),
            retryable: false,
        };
        let s = fb.format_for_llm();
        assert!(s.contains("[tool_error]"));
        assert!(s.contains("invalid_parameters"));
        assert!(s.contains("missing required field: url"));
        assert!(s.contains("retryable: false"));
    }

    #[test]
    fn all_categories_have_labels() {
        let categories = [
            ToolErrorCategory::ToolNotFound,
            ToolErrorCategory::InvalidParameters,
            ToolErrorCategory::TypeMismatch,
            ToolErrorCategory::PolicyBlocked,
            ToolErrorCategory::ConfirmationRequired,
            ToolErrorCategory::PermanentFailure,
            ToolErrorCategory::Cancelled,
            ToolErrorCategory::RateLimited,
            ToolErrorCategory::ServerError,
            ToolErrorCategory::NetworkError,
            ToolErrorCategory::Timeout,
        ];
        for cat in categories {
            assert!(!cat.label().is_empty(), "category {cat:?} has empty label");
            assert!(
                !cat.suggestion().is_empty(),
                "category {cat:?} has empty suggestion"
            );
        }
    }

    // ── classify_http_status: full coverage per taxonomy spec ────────────────

    #[test]
    fn classify_http_400_is_invalid_parameters() {
        assert_eq!(
            classify_http_status(400),
            ToolErrorCategory::InvalidParameters
        );
    }

    #[test]
    fn classify_http_401_is_policy_blocked() {
        assert_eq!(classify_http_status(401), ToolErrorCategory::PolicyBlocked);
    }

    #[test]
    fn classify_http_502_is_server_error() {
        assert_eq!(classify_http_status(502), ToolErrorCategory::ServerError);
    }

    // ── ToolErrorFeedback: category-specific content ──────────────────────────

    #[test]
    fn feedback_permanent_failure_not_retryable() {
        let fb = ToolErrorFeedback {
            category: ToolErrorCategory::PermanentFailure,
            message: "resource does not exist".to_owned(),
            retryable: false,
        };
        let s = fb.format_for_llm();
        assert!(s.contains("permanent_failure"));
        assert!(s.contains("resource does not exist"));
        assert!(s.contains("retryable: false"));
        // Suggestion must not mention auto-retry for a permanent error.
        let suggestion = ToolErrorCategory::PermanentFailure.suggestion();
        assert!(!suggestion.contains("retry automatically"), "{suggestion}");
    }

    #[test]
    fn feedback_rate_limited_is_retryable_and_mentions_retry() {
        let fb = ToolErrorFeedback {
            category: ToolErrorCategory::RateLimited,
            message: "too many requests".to_owned(),
            retryable: true,
        };
        let s = fb.format_for_llm();
        assert!(s.contains("rate_limited"));
        assert!(s.contains("retryable: true"));
        // RateLimited suggestion must mention automatic retry.
        let suggestion = ToolErrorCategory::RateLimited.suggestion();
        assert!(suggestion.contains("retry automatically"), "{suggestion}");
    }

    // ── B4 regression: infrastructure errors must NOT be quality failures ─────

    #[test]
    fn b4_infrastructure_errors_not_quality_failures() {
        // These categories must never trigger self-reflection (B4 fix).
        for cat in [
            ToolErrorCategory::NetworkError,
            ToolErrorCategory::ServerError,
            ToolErrorCategory::RateLimited,
            ToolErrorCategory::Timeout,
        ] {
            assert!(
                !cat.is_quality_failure(),
                "{cat:?} must not be a quality failure"
            );
            // And they must be retryable.
            assert!(cat.is_retryable(), "{cat:?} must be retryable");
        }
    }

    #[test]
    fn b4_quality_failures_may_trigger_reflection() {
        // These categories should trigger self-reflection.
        for cat in [
            ToolErrorCategory::InvalidParameters,
            ToolErrorCategory::TypeMismatch,
            ToolErrorCategory::ToolNotFound,
        ] {
            assert!(
                cat.is_quality_failure(),
                "{cat:?} must be a quality failure"
            );
            // Quality failures are not retryable.
            assert!(!cat.is_retryable(), "{cat:?} must not be retryable");
        }
    }

    // ── B2 regression: io::NotFound must NOT produce ToolNotFound ────────────

    #[test]
    fn b2_io_not_found_maps_to_permanent_failure_not_tool_not_found() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "bash: command not found");
        let cat = classify_io_error(&err);
        assert_ne!(
            cat,
            ToolErrorCategory::ToolNotFound,
            "OS-level NotFound must NOT map to ToolNotFound"
        );
        assert_eq!(
            cat,
            ToolErrorCategory::PermanentFailure,
            "OS-level NotFound must map to PermanentFailure"
        );
    }

    // ── ToolErrorCategory::Cancelled: not retryable, not quality failure ──────

    #[test]
    fn cancelled_is_not_retryable_and_not_quality_failure() {
        assert!(!ToolErrorCategory::Cancelled.is_retryable());
        assert!(!ToolErrorCategory::Cancelled.is_quality_failure());
        assert!(!ToolErrorCategory::Cancelled.needs_parameter_reformat());
    }
}
