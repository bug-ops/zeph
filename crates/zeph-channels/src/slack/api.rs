//! Slack Web API client for message operations.

use serde::{Deserialize, Serialize};
use serde_json::Value;

const SLACK_API: &str = "https://slack.com/api";
const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;

pub struct SlackApi {
    client: reqwest::Client,
    token: String,
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
            client: reqwest::Client::new(),
            token,
        }
    }

    /// Call auth.test to retrieve the bot's own user ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP request or Slack API fails.
    pub async fn auth_test(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let resp: Value = self
            .client
            .post(format!("{SLACK_API}/auth.test"))
            .bearer_auth(&self.token)
            .send()
            .await?
            .json()
            .await?;

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
    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let resp: SlackResponse = self
            .client
            .post(format!("{SLACK_API}/chat.postMessage"))
            .bearer_auth(&self.token)
            .json(&PostMessage { channel, text })
            .send()
            .await?
            .json()
            .await?;

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
    pub async fn update_message(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let resp: SlackResponse = self
            .client
            .post(format!("{SLACK_API}/chat.update"))
            .bearer_auth(&self.token)
            .json(&UpdateMessage { channel, ts, text })
            .send()
            .await?
            .json()
            .await?;

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

        let resp = self.client.get(url).bearer_auth(&self.token).send().await?;
        if !resp.status().is_success() {
            return Err(format!("slack file download failed: {}", resp.status()).into());
        }
        let bytes = resp.bytes().await?;
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
