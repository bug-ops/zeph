// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared HTTP 429 retry-with-backoff helper for channel REST clients.
//!
//! Discord, Telegram, and Slack all rate-limit via HTTP 429 with a
//! `Retry-After` header (or, for Discord and some Telegram responses, a JSON
//! body field of the same name). This module centralises the retry loop so
//! each channel's REST client only has to build the request.

use std::time::Duration;

use serde::Deserialize;

/// Upper bound applied to any `Retry-After` value, whether from the header or body.
const MAX_RETRY_SECS: f64 = 60.0;
/// Maximum number of retry attempts before giving up and surfacing the error.
const MAX_RETRIES: u32 = 3;

/// Body shape used by rate-limit responses that carry `retry_after` as JSON.
#[derive(Deserialize, Default)]
struct RateLimitBody {
    retry_after: Option<f64>,
}

/// Executes a request with automatic 429 retry-after backoff.
///
/// Builds a fresh request each attempt via `make_req`. On HTTP 429 the function
/// reads the `Retry-After` header (falling back to the JSON body's `retry_after`
/// field), clamps to [`MAX_RETRY_SECS`], sleeps, and retries up to [`MAX_RETRIES`]
/// times. When retries are exhausted a final request is issued to obtain a
/// `reqwest::Error` with the original HTTP status — `reqwest::Error` cannot be
/// constructed directly.
///
/// `context` is a short label (e.g. `"discord"`, `"telegram"`, `"slack"`) attached
/// to the warning logs so retries can be attributed to the originating channel.
///
/// # Timing
///
/// Under sustained 429s, the worst-case wall-clock across all attempts is
/// approximately `(MAX_RETRIES + 2) * <per-request timeout>` (each attempt's
/// own `send()`, including the final un-retried attempt) plus
/// `MAX_RETRIES * MAX_RETRY_SECS` (the backoff sleeps between them) — with the
/// current constants, on the order of minutes, not seconds. Callers that await
/// this on a hot path without an outer timeout should account for this.
///
/// # Errors
///
/// Returns a [`reqwest::Error`] when all retries are exhausted, a non-429 HTTP
/// error is received, or the request's own timeout is exceeded.
pub(crate) async fn send_with_retry<F>(
    context: &str,
    make_req: F,
) -> Result<reqwest::Response, reqwest::Error>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut attempts = 0u32;
    loop {
        let resp = make_req().send().await?;

        if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
            return resp.error_for_status();
        }

        // Parse retry delay: header wins, then body field, then default 1 s.
        let header_secs = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<f64>().ok());

        let body_secs = resp
            .json::<RateLimitBody>()
            .await
            .unwrap_or_default()
            .retry_after;

        let delay_secs = header_secs.or(body_secs).unwrap_or(1.0).min(MAX_RETRY_SECS);

        attempts += 1;
        if attempts > MAX_RETRIES {
            tracing::warn!(
                context,
                delay_secs,
                attempts,
                "rate-limited and retries exhausted"
            );
            // Surface as an error by issuing the request once more without retry.
            return make_req().send().await?.error_for_status();
        }

        tracing::warn!(
            context,
            delay_secs,
            attempt = attempts,
            max = MAX_RETRIES,
            "rate-limited (429), backing off"
        );
        tokio::time::sleep(Duration::from_secs_f64(delay_secs)).await;
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn send_with_retry_succeeds_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/ch1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "1"})))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/channels/ch1/messages", server.uri());
        let resp = send_with_retry("test", || client.post(&url)).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn send_with_retry_retries_on_429_then_succeeds() {
        let server = MockServer::start().await;

        // First call → 429 with Retry-After header.
        Mock::given(method("POST"))
            .and(path("/channels/ch1/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .append_header("Retry-After", "0")
                    .set_body_json(serde_json::json!({"retry_after": 0.0})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second call → 200.
        Mock::given(method("POST"))
            .and(path("/channels/ch1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "2"})))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/channels/ch1/messages", server.uri());
        let resp = send_with_retry("test", || client.post(&url)).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn send_with_retry_uses_body_retry_after_when_no_header() {
        let server = MockServer::start().await;

        // Three 429 responses without Retry-After header but with body field.
        Mock::given(method("POST"))
            .and(path("/channels/ch1/messages"))
            .respond_with(
                ResponseTemplate::new(429).set_body_json(serde_json::json!({"retry_after": 0.0})),
            )
            .up_to_n_times(3)
            .mount(&server)
            .await;

        // Fourth → 200.
        Mock::given(method("POST"))
            .and(path("/channels/ch1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "3"})))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/channels/ch1/messages", server.uri());
        let resp = send_with_retry("test", || client.post(&url)).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn send_with_retry_propagates_non_429_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/ch1/messages"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/channels/ch1/messages", server.uri());
        let result = send_with_retry("test", || client.post(&url)).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.status(), Some(reqwest::StatusCode::FORBIDDEN));
    }

    #[tokio::test]
    async fn send_with_retry_errors_when_retries_exhausted() {
        let server = MockServer::start().await;

        // Return 429 for all requests — exhausts MAX_RETRIES (3) then the final attempt.
        Mock::given(method("POST"))
            .and(path("/channels/ch1/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .append_header("Retry-After", "0")
                    .set_body_json(serde_json::json!({"retry_after": 0.0})),
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/channels/ch1/messages", server.uri());
        let result = send_with_retry("test", || client.post(&url)).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.status(), Some(reqwest::StatusCode::TOO_MANY_REQUESTS));
    }

    #[test]
    fn rate_limit_body_defaults_to_none() {
        let body: RateLimitBody = serde_json::from_str("{}").unwrap();
        assert!(body.retry_after.is_none());
    }

    #[test]
    fn rate_limit_body_parses_float() {
        let body: RateLimitBody = serde_json::from_str(r#"{"retry_after": 1.5}"#).unwrap();
        assert!((body.retry_after.unwrap() - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn max_retry_secs_clamps() {
        let unclamped: f64 = 120.0;
        assert_eq!(
            unclamped.min(MAX_RETRY_SECS).to_bits(),
            MAX_RETRY_SECS.to_bits()
        );
    }
}
