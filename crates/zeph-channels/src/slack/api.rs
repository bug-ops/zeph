// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Slack Web API client for message operations.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

const SLACK_API: &str = "https://slack.com/api";
const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;
/// Per-request HTTP timeout applied to every Slack Web API call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub struct SlackApi {
    client: reqwest::Client,
    token: String,
    /// Base URL for the Slack Web API. Always [`SLACK_API`] outside of tests.
    base_url: String,
}

impl std::fmt::Debug for SlackApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackApi")
            .field("token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct SlackResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    ts: Option<String>,
}

#[derive(Serialize)]
struct PostMessage<'a> {
    channel: &'a str,
    text: &'a str,
}

#[derive(Serialize)]
struct UpdateMessage<'a> {
    channel: &'a str,
    ts: &'a str,
    text: &'a str,
}

impl SlackApi {
    #[must_use]
    pub fn new(token: String) -> Self {
        Self {
            client: zeph_core::http::default_client(),
            token,
            base_url: SLACK_API.to_string(),
        }
    }

    /// Test-only constructor pointing at a custom base URL (e.g. a mock server).
    #[cfg(test)]
    fn with_base_url(base_url: String, token: String) -> Self {
        Self {
            client: zeph_core::http::default_client(),
            token,
            base_url,
        }
    }

    /// Call auth.test to retrieve the bot's own user ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request or Slack API fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.slack.auth_test", skip_all)
    )]
    pub async fn auth_test(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/auth.test", self.base_url);
        let resp: Value = crate::common::http_retry::send_with_retry("slack", || {
            self.client
                .post(&url)
                .bearer_auth(&self.token)
                .timeout(REQUEST_TIMEOUT)
        })
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
        .json()
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        if resp.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(format!("slack auth.test: {err}").into());
        }
        resp.get("user_id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| "no user_id in auth.test response".into())
    }

    /// Post a new message, returning the message timestamp (ts).
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request or Slack API fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.slack.post_message", skip_all)
    )]
    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/chat.postMessage", self.base_url);
        let body = PostMessage { channel, text };
        let resp: SlackResponse = crate::common::http_retry::send_with_retry("slack", || {
            self.client
                .post(&url)
                .bearer_auth(&self.token)
                .timeout(REQUEST_TIMEOUT)
                .json(&body)
        })
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
        .json()
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        if !resp.ok {
            return Err(
                format!("slack chat.postMessage: {}", resp.error.unwrap_or_default()).into(),
            );
        }
        resp.ts.ok_or_else(|| "no ts in response".into())
    }

    /// Update an existing message.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request or Slack API fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.slack.update_message", skip_all)
    )]
    pub async fn update_message(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/chat.update", self.base_url);
        let body = UpdateMessage { channel, ts, text };
        let resp: SlackResponse = crate::common::http_retry::send_with_retry("slack", || {
            self.client
                .post(&url)
                .bearer_auth(&self.token)
                .timeout(REQUEST_TIMEOUT)
                .json(&body)
        })
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
        .json()
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        if !resp.ok {
            return Err(format!("slack chat.update: {}", resp.error.unwrap_or_default()).into());
        }
        Ok(())
    }

    /// Download a file from Slack using the bot token for authorization.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request fails or the response status is not success.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.slack.download_file", skip_all)
    )]
    pub async fn download_file(
        &self,
        url: &str,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        let host = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(String::from));
        if !host.is_some_and(|h| h.ends_with(".slack.com")) {
            return Err(format!("refusing to send token to non-slack host: {url}").into());
        }

        let resp = crate::common::http_retry::send_with_retry("slack", || {
            self.client
                .get(url)
                .bearer_auth(&self.token)
                .timeout(REQUEST_TIMEOUT)
        })
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        let bytes = tokio::time::timeout(Duration::from_secs(15), resp.bytes())
            .await
            .map_err(|_| "slack file download body timed out")?
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        if bytes.len() > MAX_AUDIO_BYTES {
            return Err(format!(
                "slack file too large: {} bytes (max {MAX_AUDIO_BYTES})",
                bytes.len()
            )
            .into());
        }
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn slack_api_debug_redacts_token() {
        let api = SlackApi::new("xoxb-secret-token".into());
        let debug = format!("{api:?}");
        assert!(!debug.contains("xoxb-secret-token"));
        assert!(debug.contains("REDACTED"));
    }

    // Generic 429 retry-with-backoff behavior is covered by
    // `crate::common::http_retry`'s own test suite. This test only proves
    // that `post_message` is wired to retry rather than failing immediately.
    #[tokio::test]
    async fn post_message_retries_on_429_then_succeeds() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(429)
                    .append_header("Retry-After", "0")
                    .set_body_json(serde_json::json!({"retry_after": 0.0})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "ts": "1234.5678"
            })))
            .mount(&server)
            .await;

        let api = SlackApi::with_base_url(server.uri(), "xoxb-test".into());
        let ts = api.post_message("C123", "hello").await.unwrap();
        assert_eq!(ts, "1234.5678");
    }

    // Regression coverage for the SSRF-style host guard in `download_file`:
    // the bot token must never be sent to a non-`*.slack.com` host. This
    // rejects before any network call, so no mock server is needed.
    #[tokio::test]
    async fn download_file_rejects_non_slack_host() {
        let api = SlackApi::new("xoxb-test".into());
        let err = api.download_file("https://evil.com/x").await.unwrap_err();
        assert!(
            err.to_string()
                .contains("refusing to send token to non-slack host"),
            "expected host-guard rejection, got: {err}"
        );
    }
}
