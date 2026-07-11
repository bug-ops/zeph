// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Discord REST API client for message operations.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::common::http_retry::send_with_retry;

const BASE_URL: &str = "https://discord.com/api/v10";
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
        let resp = send_with_retry("discord", || {
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
        send_with_retry("discord", || {
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
        send_with_retry("discord", || {
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
    use super::*;

    // Generic 429 retry-with-backoff behavior is covered by
    // `crate::common::http_retry`'s own test suite. Only Discord-specific
    // behavior is tested here.

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
