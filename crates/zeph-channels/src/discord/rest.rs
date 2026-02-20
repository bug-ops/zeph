//! Discord REST API client for message operations.

use serde::{Deserialize, Serialize};

const BASE_URL: &str = "https://discord.com/api/v10";

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
    /// Returns an error if the HTTP request fails.
    pub async fn send_message(
        &self,
        channel_id: &str,
        content: &str,
    ) -> Result<DiscordMessage, reqwest::Error> {
        self.client
            .post(format!("{BASE_URL}/channels/{channel_id}/messages"))
            .header("Authorization", self.auth_header())
            .json(&CreateMessage { content })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }

    /// # Errors
    ///
    /// Returns an error if the HTTP request fails.
    pub async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<(), reqwest::Error> {
        self.client
            .patch(format!(
                "{BASE_URL}/channels/{channel_id}/messages/{message_id}"
            ))
            .header("Authorization", self.auth_header())
            .json(&EditMessage { content })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// # Errors
    ///
    /// Returns an error if the HTTP request fails.
    pub async fn trigger_typing(&self, channel_id: &str) -> Result<(), reqwest::Error> {
        self.client
            .post(format!("{BASE_URL}/channels/{channel_id}/typing"))
            .header("Authorization", self.auth_header())
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}
