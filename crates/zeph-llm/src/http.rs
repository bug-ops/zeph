// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared HTTP client construction for consistent timeout and TLS configuration.

use std::time::Duration;

use crate::error::{LlmError, body_is_context_length_error};

/// Create an HTTP client for LLM inference providers.
///
/// Connect timeout is fixed at 30s. `request_timeout_secs` is a hard backstop
/// for the full HTTP round-trip; it should be set larger than the agent-level
/// `TimeoutConfig.llm_seconds` so the tokio-layer fires first in normal
/// operation and this only catches runaway requests.
///
/// # Panics
///
/// Panics if the underlying TLS configuration cannot be initialized, which
/// should never happen in a correctly compiled binary.
#[must_use]
pub fn llm_client(request_timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(request_timeout_secs))
        .user_agent(concat!("zeph/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("LLM HTTP client construction must not fail")
}

/// Maps a non-2xx HTTP status and response body to the appropriate [`LlmError`].
///
/// Returns [`LlmError::ContextLengthExceeded`] when `status` is `400 Bad Request`
/// and `body` matches known context-length-exceeded error patterns.
///
/// Returns [`LlmError::ApiError`] for all other non-2xx responses. This covers
/// authentication errors, server errors, and unexpected 4xx codes that are not
/// context-length failures.
///
/// Read the response body text and check the HTTP status code in one step.
///
/// Returns the body text on success. On non-2xx responses, logs the error at
/// `error` level and returns the appropriate [`LlmError`] via [`map_error_response`].
///
/// Use this for call sites where the only error action is returning
/// `map_error_response(status, text, provider)` — i.e. no retry logic or
/// custom error variant selection based on the status.
pub(crate) async fn check_response(
    response: reqwest::Response,
    provider: &str,
) -> Result<String, LlmError> {
    let status = response.status();
    let text = response.text().await.map_err(LlmError::Http)?;
    if !status.is_success() {
        tracing::error!(%provider, %status, body = %text, "API error response");
        return Err(map_error_response(status, &text, provider));
    }
    Ok(text)
}

/// Extract the HTTP status code and response body text together.
///
/// Use when the caller needs to inspect both `status` and `text` before
/// deciding the error path — for example, retry logic, `InvalidInput`
/// variants based on status code, or custom error messages.
pub(crate) async fn read_response_body(
    response: reqwest::Response,
) -> Result<(reqwest::StatusCode, String), LlmError> {
    let status = response.status();
    let text = response.text().await.map_err(LlmError::Http)?;
    Ok((status, text))
}

/// Only call this function after confirming the response is not a 2xx success.
pub(crate) fn map_error_response(
    status: reqwest::StatusCode,
    body: &str,
    provider: &str,
) -> LlmError {
    if status == reqwest::StatusCode::BAD_REQUEST && body_is_context_length_error(body) {
        LlmError::ContextLengthExceeded
    } else {
        LlmError::ApiError {
            provider: provider.into(),
            status: status.as_u16(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_request_with_context_length_body_returns_exceeded() {
        let err = map_error_response(
            reqwest::StatusCode::BAD_REQUEST,
            "context length exceeded",
            "p",
        );
        assert!(matches!(err, LlmError::ContextLengthExceeded));
    }

    #[test]
    fn bad_request_without_context_body_returns_api_error() {
        let err = map_error_response(
            reqwest::StatusCode::BAD_REQUEST,
            "invalid parameter",
            "myprovider",
        );
        assert!(matches!(
            err,
            LlmError::ApiError { ref provider, status: 400 } if provider == "myprovider"
        ));
    }

    #[test]
    fn server_error_returns_api_error() {
        let err = map_error_response(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "", "svc");
        assert!(matches!(
            err,
            LlmError::ApiError { ref provider, status: 500 } if provider == "svc"
        ));
    }

    #[test]
    fn unauthorized_returns_api_error() {
        let err = map_error_response(reqwest::StatusCode::UNAUTHORIZED, "", "claude");
        assert!(matches!(
            err,
            LlmError::ApiError { ref provider, status: 401 } if provider == "claude"
        ));
    }

    #[test]
    fn too_many_requests_returns_api_error() {
        let err = map_error_response(reqwest::StatusCode::TOO_MANY_REQUESTS, "", "openai");
        assert!(matches!(
            err,
            LlmError::ApiError { ref provider, status: 429 } if provider == "openai"
        ));
    }
}
