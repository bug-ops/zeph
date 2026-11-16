// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared HTTP client construction for consistent timeout and TLS configuration.

use std::time::Duration;

use zeph_common::http::{BodyLimit, BodyReadError, read_capped};

use crate::error::{LlmError, body_is_context_length_error};

/// Maximum accepted size of one provider response body.
///
/// Provider endpoints can be third-party (`compatible`, `cocoon`, `gonka`); the cap bounds memory
/// regardless of what the peer streams inside the request timeout.
pub(crate) const LLM_BODY_LIMIT: BodyLimit = BodyLimit::from_const(32 * 1024 * 1024);

/// Read a response body as text, failing once it exceeds [`LLM_BODY_LIMIT`].
///
/// Invalid UTF-8 is replaced lossily, matching `reqwest::Response::text`.
pub(crate) async fn read_body(response: reqwest::Response) -> Result<String, LlmError> {
    let bytes = read_capped(response, LLM_BODY_LIMIT)
        .await
        .map_err(|e| match e {
            BodyReadError::TooLarge { limit } => LlmError::ResponseTooLarge { limit },
            BodyReadError::Transport(e) => LlmError::Http(e),
        })?;
    Ok(String::from_utf8(bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// Read a response body through [`read_body`] and decode it as JSON.
///
/// Replaces `reqwest::Response::json`, which buffers the whole body without a size cap.
pub(crate) async fn read_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, LlmError> {
    let text = read_body(response).await?;
    serde_json::from_str(&text).map_err(LlmError::Json)
}

/// Map a non-2xx speech-to-text response to [`LlmError::TranscriptionFailed`].
///
/// The body is read through the capped reader, scrubbed of secret shapes, stripped of control
/// characters and cut on a character boundary at 500 characters. A body over the cap yields
/// [`LlmError::ResponseTooLarge`] instead.
pub(crate) async fn transcription_failure(response: reqwest::Response) -> LlmError {
    let status = response.status();
    match read_body(response).await {
        Ok(body) => LlmError::TranscriptionFailed(format!(
            "{status}: {}",
            crate::error::sanitize_provider_text(&body, 500)
        )),
        Err(e) => e,
    }
}

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
    let text = read_body(response).await?;
    if !status.is_success() {
        log_api_error_body(provider, status, &text);
        return Err(map_error_response(status, &text, provider));
    }
    Ok(text)
}

/// Log a raw API error body at `debug` level with secret shapes scrubbed.
///
/// The user-visible cause travels in [`LlmError::ApiError`]; the raw body is diagnostic only, so
/// it must not add an ERROR line per layer.
pub(crate) fn log_api_error_body(provider: &str, status: reqwest::StatusCode, body: &str) {
    let scrubbed = zeph_common::secret_redact::scrub_secret_shapes(body);
    tracing::debug!(%provider, %status, body = %scrubbed, "API error response");
}

/// Log the scrubbed `body` at debug level and map the non-2xx `status` through
/// [`map_error_response`].
///
/// The full capped body is classified and sanitized by [`LlmError::api_error`]; callers must not
/// pre-truncate it, or a secret straddling the cut escapes the redaction pattern.
pub(crate) fn api_failure(provider: &str, status: reqwest::StatusCode, body: &str) -> LlmError {
    log_api_error_body(provider, status, body);
    map_error_response(status, body, provider)
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
    let text = read_body(response).await?;
    Ok((status, text))
}

/// Map an HTTP 400 body to the matching [`LlmError`], logging the scrubbed body at debug level.
///
/// Context-length bodies become [`LlmError::ContextLengthExceeded`]; everything else becomes a
/// sanitized [`LlmError::InvalidInput`].
pub(crate) fn map_bad_request(provider: &str, body: &str) -> LlmError {
    log_api_error_body(provider, reqwest::StatusCode::BAD_REQUEST, body);
    if body_is_context_length_error(body) {
        LlmError::ContextLengthExceeded
    } else {
        LlmError::invalid_input_from_body(provider, body)
    }
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
        LlmError::api_error(provider, status.as_u16(), body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[tokio::test]
    async fn read_body_rejects_body_over_limit() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![b'a'; LLM_BODY_LIMIT.get() + 1]),
            )
            .mount(&server)
            .await;
        let response = reqwest::get(server.uri()).await.unwrap();
        assert_matches!(
            read_body(response).await,
            Err(LlmError::ResponseTooLarge { limit }) if limit == LLM_BODY_LIMIT.get()
        );
    }

    #[tokio::test]
    async fn read_body_returns_text_under_limit() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
            .mount(&server)
            .await;
        let response = reqwest::get(server.uri()).await.unwrap();
        assert_eq!(read_body(response).await.unwrap(), "hello");
    }

    #[test]
    fn map_bad_request_sanitizes_body() {
        let body = format!(
            "key sk-abcdef0123456789abcdef0123456789abcdef \x1b[31m{}",
            "x".repeat(5000)
        );
        let LlmError::InvalidInput { message, .. } = map_bad_request("p", &body) else {
            panic!("expected InvalidInput");
        };
        assert!(!message.contains("sk-abcdef"));
        assert!(!message.chars().any(char::is_control));
        assert!(message.chars().count() <= crate::error::ApiErrorDetail::MAX_MESSAGE_CHARS);
    }

    #[test]
    fn map_bad_request_keeps_context_length_classification() {
        assert_matches!(
            map_bad_request("p", "maximum context length exceeded"),
            LlmError::ContextLengthExceeded
        );
    }

    #[test]
    fn bad_request_with_context_length_body_returns_exceeded() {
        let err = map_error_response(
            reqwest::StatusCode::BAD_REQUEST,
            "context length exceeded",
            "p",
        );
        assert_matches!(err, LlmError::ContextLengthExceeded);
    }

    #[test]
    fn bad_request_without_context_body_returns_api_error() {
        let err = map_error_response(
            reqwest::StatusCode::BAD_REQUEST,
            "invalid parameter",
            "myprovider",
        );
        assert_matches!(
            err,
            LlmError::ApiError { ref provider, status: 400, .. } if provider == "myprovider"
        );
    }

    #[test]
    fn server_error_returns_api_error() {
        let err = map_error_response(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "", "svc");
        assert_matches!(
            err,
            LlmError::ApiError { ref provider, status: 500, .. } if provider == "svc"
        );
    }

    #[test]
    fn unauthorized_returns_api_error() {
        let err = map_error_response(reqwest::StatusCode::UNAUTHORIZED, "", "claude");
        assert_matches!(
            err,
            LlmError::ApiError { ref provider, status: 401, .. } if provider == "claude"
        );
    }

    #[test]
    fn too_many_requests_returns_api_error() {
        let err = map_error_response(reqwest::StatusCode::TOO_MANY_REQUESTS, "", "openai");
        assert_matches!(
            err,
            LlmError::ApiError { ref provider, status: 429, .. } if provider == "openai"
        );
    }
}
