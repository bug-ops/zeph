// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! 12-category tool invocation error taxonomy (arXiv:2601.16280).
//!
//! The [`ToolErrorCategory`] and [`ErrorDomain`] enums are defined in `zeph-common` and
//! re-exported here for backwards compatibility. Tool-specific helpers (`classify_http_status`,
//! `classify_io_error`, `ToolErrorFeedback`) remain in this module.

pub use zeph_common::error_taxonomy::{ErrorDomain, ToolErrorCategory, ToolInvocationPhase};

use crate::executor::ErrorKind;

/// Extension trait adding `zeph-tools`-specific methods to [`ToolErrorCategory`].
///
/// This trait exists because `ToolErrorCategory` is defined in `zeph-common` but
/// `ErrorKind` is defined in `zeph-tools`. Callers in `zeph-tools` use
/// `use crate::error_taxonomy::ToolErrorCategoryExt` to access these methods.
pub trait ToolErrorCategoryExt {
    /// Coarse classification for backward compatibility with existing `ErrorKind`.
    fn error_kind(self) -> ErrorKind;
}

impl ToolErrorCategoryExt for ToolErrorCategory {
    fn error_kind(self) -> ErrorKind {
        if self.is_retryable() {
            ErrorKind::Transient
        } else {
            ErrorKind::Permanent
        }
    }
}

/// Structured error feedback injected as `tool_result` content for classified errors.
///
/// Provides the LLM with actionable information about what went wrong and what to
/// do next, replacing the opaque `[error] ...` string format.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolErrorFeedback {
    /// Fine-grained category of the error (used to select suggestion and label).
    pub category: ToolErrorCategory,
    /// Human-readable error message from the failing executor.
    pub message: String,
    /// Whether the agent loop should attempt an automatic retry for this error.
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
    use super::ToolErrorCategoryExt as _;
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
        assert_eq!(
            ToolErrorCategory::NetworkError.error_kind(),
            ErrorKind::Transient
        );
        assert_eq!(
            ToolErrorCategory::Timeout.error_kind(),
            ErrorKind::Transient
        );
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
    fn phase_setup_for_tool_not_found() {
        assert_eq!(
            ToolErrorCategory::ToolNotFound.phase(),
            ToolInvocationPhase::Setup
        );
    }

    #[test]
    fn phase_param_handling() {
        assert_eq!(
            ToolErrorCategory::InvalidParameters.phase(),
            ToolInvocationPhase::ParamHandling
        );
        assert_eq!(
            ToolErrorCategory::TypeMismatch.phase(),
            ToolInvocationPhase::ParamHandling
        );
    }
}
