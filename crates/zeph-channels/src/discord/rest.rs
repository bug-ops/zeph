// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Discord REST API client for message operations.

use std::time::Duration;

use serde::{Deserialize, Serialize};

const BASE_URL: &str = "https://discord.com/api/v10";
const MAX_RETRY_SECS: f64 = 60.0;
const MAX_RETRIES: u32 = 3;
/// Per-request HTTP timeout applied to every Discord REST call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
struct CurrentApplication {
    id: String,
}

#[derive(Serialize)]
struct SlashCommand {
    name: &'static str,
    description: &'static str,
    #[serde(rename = "type")]
    kind: u8,
}

/// Slash commands to register with Discord at bot startup.
const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "reset",
        description: "Reset conversation history",
        kind: 1,
    },
    SlashCommand {
        name: "skills",
        description: "List loaded skills",
        kind: 1,
    },
    SlashCommand {
        name: "agent",
        description: "Manage sub-agents",
        kind: 1,
    },
];

#[derive(Clone)]
pub struct RestClient {
    client: reqwest::Client,
    token: String,
}

impl std::fmt::Debug for RestClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestClient")
            .field("token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
pub struct DiscordMessage {
    pub id: String,
}

#[derive(Serialize)]
struct CreateMessage<'a> {
    content: &'a str,
}

#[derive(Serialize)]
struct EditMessage<'a> {
    content: &'a str,
}

/// Body returned by Discord on HTTP 429.
#[derive(Deserialize, Default)]
struct RateLimitBody {
    retry_after: Option<f64>,
}

/// Executes a request with automatic 429 retry-after backoff.
///
/// Builds a fresh request each attempt via `make_req`. On HTTP 429 the function
/// reads `Retry-After` header (falling back to the JSON body `retry_after` field),
/// clamps to [`MAX_RETRY_SECS`], sleeps, and retries up to [`MAX_RETRIES`] times.
/// When retries are exhausted a final request is issued to obtain a `reqwest::Error`
/// with the original HTTP status — `reqwest::Error` cannot be constructed directly.
///
/// # Errors
///
/// Returns a [`reqwest::Error`] when all retries are exhausted, a non-429 HTTP error
/// is received, or the per-request timeout ([`REQUEST_TIMEOUT`]) is exceeded.
async fn send_with_retry<F>(make_req: F) -> Result<reqwest::Response, reqwest::Error>
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
                delay_secs,
                attempts,
                "discord: rate-limited and retries exhausted"
            );
            // Surface as an error by issuing the request once more without retry.
            return make_req().send().await?.error_for_status();
        }

        tracing::warn!(
            delay_secs,
            attempt = attempts,
            max = MAX_RETRIES,
            "discord: rate-limited (429), backing off"
        );
        tokio::time::sleep(Duration::from_secs_f64(delay_secs)).await;
    }
}

impl RestClient {
    #[must_use]
    pub fn new(token: String) -> Self {
        let client = zeph_core::http::default_client();
        Self { client, token }
    }

    fn auth_header(&self) -> String {
        format!("Bot {}", self.token)
    }

    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or rate-limit retries are exhausted.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.discord.rest.send_message", skip_all)
    )]
    pub async fn send_message(
        &self,
        channel_id: &str,
        content: &str,
    ) -> Result<DiscordMessage, reqwest::Error> {
        let url = format!("{BASE_URL}/channels/{channel_id}/messages");
        let auth = self.auth_header();
        let resp = send_with_retry(|| {
            self.client
                .post(&url)
                .header("Authorization", &auth)
                .timeout(REQUEST_TIMEOUT)
                .json(&CreateMessage { content })
        })
        .await?;
        resp.json().await
    }

    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or rate-limit retries are exhausted.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.discord.rest.edit_message", skip_all)
    )]
    pub async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<(), reqwest::Error> {
        let url = format!("{BASE_URL}/channels/{channel_id}/messages/{message_id}");
        let auth = self.auth_header();
        send_with_retry(|| {
            self.client
                .patch(&url)
                .header("Authorization", &auth)
                .timeout(REQUEST_TIMEOUT)
                .json(&EditMessage { content })
        })
        .await?;
        Ok(())
    }

    /// Register global slash commands for this bot application.
    ///
    /// Uses `PUT /applications/{id}/commands` which is idempotent — safe to call on every
    /// restart. Global commands take up to 1 hour to propagate. Logs success or failure;
    /// never returns an error (fire-and-forget caller pattern).
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.discord.rest.register_slash_commands", skip_all)
    )]
    pub async fn register_slash_commands(&self) {
        let app_id = match self
            .client
            .get(format!("{BASE_URL}/applications/@me"))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        {
            Ok(resp) => match resp.json::<CurrentApplication>().await {
                Ok(app) => app.id,
                Err(e) => {
                    tracing::warn!("discord: failed to parse application info: {e}");
                    return;
                }
            },
            Err(e) => {
                tracing::warn!("discord: failed to fetch application info: {e}");
                return;
            }
        };

        match self
            .client
            .put(format!("{BASE_URL}/applications/{app_id}/commands"))
            .header("Authorization", self.auth_header())
            .json(SLASH_COMMANDS)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        {
            Ok(_) => tracing::info!("discord: slash commands registered successfully"),
            Err(e) => tracing::warn!("discord: slash command registration failed: {e}"),
        }
    }

    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or rate-limit retries are exhausted.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.discord.rest.trigger_typing", skip_all)
    )]
    pub async fn trigger_typing(&self, channel_id: &str) -> Result<(), reqwest::Error> {
        let url = format!("{BASE_URL}/channels/{channel_id}/typing");
        let auth = self.auth_header();
        send_with_retry(|| {
            self.client
                .post(&url)
                .header("Authorization", &auth)
                .timeout(REQUEST_TIMEOUT)
        })
        .await?;
        Ok(())
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
        let resp = send_with_retry(|| client.post(&url)).await.unwrap();
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
        let resp = send_with_retry(|| client.post(&url)).await.unwrap();
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
        let resp = send_with_retry(|| client.post(&url)).await.unwrap();
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
        let result = send_with_retry(|| client.post(&url)).await;
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
        let result = send_with_retry(|| client.post(&url)).await;
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

    #[test]
    fn rest_client_debug_redacts_token() {
        let rc = RestClient {
            client: reqwest::Client::new(),
            token: "secret-token".into(),
        };
        let debug = format!("{rc:?}");
        assert!(!debug.contains("secret-token"));
        assert!(debug.contains("REDACTED"));
    }
}
