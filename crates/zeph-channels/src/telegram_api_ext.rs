// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Raw HTTP client for Telegram Bot API 10.0 methods not yet exposed by teloxide.
//!
//! [`TelegramApiClient`] wraps `reqwest` and provides typed async methods for
//! Bot API 10.0 features: Guest Mode query answering, managed-bot access
//! settings, and reaction moderation.  It is embedded in [`TelegramChannel`]
//! and accessible via [`TelegramChannel::api_ext`].
//!
//! Once teloxide gains native support for these methods, this module can be
//! removed and call sites updated to use the teloxide API directly (tracked in
//! issue #3732).
//!
//! [`TelegramChannel`]: crate::telegram::TelegramChannel
//! [`TelegramChannel::api_ext`]: crate::telegram::TelegramChannel::api_ext

use serde::{Deserialize, Serialize};

/// Response payload from the `answerGuestQuery` Bot API method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SentGuestMessage {
    /// Telegram message identifier of the sent reply.
    pub message_id: i64,
    /// Identifier of the chat the message was sent to.
    pub chat_id: i64,
}

/// Access settings for a managed bot in Guest Mode.
///
/// # Note
///
/// Field names are based on the Bot API 10.0 specification draft and will be
/// verified against real API responses when `#3732` migrates to native teloxide.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BotAccessSettings {
    /// Whether the managed bot may receive private-chat messages.
    pub allow_private_chats: bool,
    /// Whether the managed bot may participate in group chats.
    pub allow_group_chats: bool,
    /// Whether the managed bot may receive channel posts.
    pub allow_channel_posts: bool,
}

/// A message received by a managed bot in Guest Mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GuestMessage {
    /// Telegram message identifier.
    pub message_id: i64,
    /// Identifier of the originating chat.
    pub from_chat_id: i64,
    /// Text content of the message, if any.
    pub text: Option<String>,
}

/// Standard Telegram API JSON response envelope.
#[derive(Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

/// Errors returned by [`TelegramApiClient`] methods.
#[derive(Debug, thiserror::Error)]
pub enum TelegramApiError {
    /// HTTP transport or status error.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    /// Telegram API returned `ok: false`.
    #[error("Telegram API error: {0}")]
    Api(String),
}

/// Raw HTTP client for Telegram Bot API 10.0 methods not covered by teloxide.
///
/// The bot token is embedded in the base URL and is never written to logs — the
/// [`Debug`] implementation redacts it.
///
/// # Examples
///
/// ```no_run
/// use zeph_channels::telegram_api_ext::TelegramApiClient;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let client = TelegramApiClient::new("123456:ABC-DEF…");
/// let result = client.answer_guest_query("query_id", "Hello!", None).await?;
/// println!("sent message_id={}", result.message_id);
/// # Ok(())
/// # }
/// ```
pub struct TelegramApiClient {
    client: reqwest::Client,
    /// `https://api.telegram.org/bot<TOKEN>` — includes the token, never log.
    base_url: String,
}

impl std::fmt::Debug for TelegramApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramApiClient")
            .field("base_url", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl TelegramApiClient {
    /// Create a new client for the given bot `token`.
    ///
    /// The base URL is set to `https://api.telegram.org/bot<TOKEN>` so that each
    /// `post()` call appends only the method name (e.g., `/answerGuestQuery`).
    ///
    /// Creates an independent `reqwest::Client` with its own connection pool.
    /// To share a connection pool with an existing client, use
    /// [`TelegramApiClient::with_client`].
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        let token = token.into();
        Self {
            client: reqwest::Client::new(),
            base_url: format!("https://api.telegram.org/bot{token}"),
        }
    }

    /// Create a client that reuses an existing `reqwest::Client`.
    ///
    /// This allows sharing a connection pool with another HTTP client — for
    /// example, the `reqwest::Client` backing teloxide's `Bot` — to avoid
    /// opening duplicate TCP connections to `api.telegram.org`.
    ///
    /// The base URL is set to `https://api.telegram.org/bot<token>` using the
    /// supplied `token`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_channels::telegram_api_ext::TelegramApiClient;
    ///
    /// let shared = reqwest::Client::new();
    /// let client = TelegramApiClient::with_client(shared, "123456:ABC-DEF…");
    /// ```
    #[must_use]
    pub fn with_client(client: reqwest::Client, token: &str) -> Self {
        Self {
            client,
            base_url: format!("https://api.telegram.org/bot{token}"),
        }
    }

    /// Create a client with a fully-qualified custom base URL.
    ///
    /// The `base_url` is stored as-is and each method name is appended with `/`.
    /// The bot token is **not** automatically embedded — the caller is responsible
    /// for including the full path prefix required by the target server.
    ///
    /// For the official Telegram Bot API protocol the expected format is:
    /// `https://api.telegram.org/bot<TOKEN>` (same as what [`new`] builds).
    /// For a local [Telegram Bot API server](https://core.telegram.org/bots/api#using-a-local-bot-api-server)
    /// the format is typically `http://localhost:8081/bot<TOKEN>`.
    ///
    /// This method is primarily intended for testing (point at a wiremock server)
    /// or for deployments that proxy through a local Bot API server.
    ///
    /// [`new`]: TelegramApiClient::new
    #[must_use]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }

    /// POST `body` to `{base_url}/{method}` and deserialize the result.
    ///
    /// Calls `.error_for_status()` before JSON parsing so that non-2xx HTTP
    /// responses (e.g. 429 rate-limit, 502 gateway error) surface as
    /// [`TelegramApiError::Http`] rather than serde deserialization errors.
    /// URLs (which contain the bot token) are stripped from any `reqwest::Error`
    /// before propagation to prevent token leakage into logs.
    #[tracing::instrument(skip(self, body), fields(method = method))]
    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        body: &impl Serialize,
    ) -> Result<T, TelegramApiError> {
        let url = format!("{}/{method}", self.base_url);
        let resp: TelegramResponse<T> = self
            .client
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|e| TelegramApiError::Http(e.without_url()))?
            .error_for_status()
            .map_err(|e| TelegramApiError::Http(e.without_url()))?
            .json()
            .await
            .map_err(|e| TelegramApiError::Http(e.without_url()))?;

        if resp.ok {
            resp.result
                .ok_or_else(|| TelegramApiError::Api("ok=true but no result".into()))
        } else {
            Err(TelegramApiError::Api(
                resp.description.unwrap_or_else(|| "unknown error".into()),
            ))
        }
    }

    /// Answer a Guest Mode query on behalf of a managed bot.
    ///
    /// # Arguments
    ///
    /// * `query_id` — identifier of the guest query to answer.
    /// * `text` — reply text.
    /// * `parse_mode` — optional parse mode (`"HTML"`, `"MarkdownV2"`, etc.).
    ///
    /// # Errors
    ///
    /// Returns [`TelegramApiError`] on HTTP failure or when `ok: false`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_channels::telegram_api_ext::TelegramApiClient;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let client = TelegramApiClient::new("TOKEN");
    /// let sent = client.answer_guest_query("qid_123", "Hello!", Some("HTML")).await?;
    /// println!("message_id={}", sent.message_id);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn answer_guest_query(
        &self,
        query_id: &str,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<SentGuestMessage, TelegramApiError> {
        #[derive(Serialize)]
        struct Req<'a> {
            guest_query_id: &'a str,
            text: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            parse_mode: Option<&'a str>,
        }
        self.post(
            "answerGuestQuery",
            &Req {
                guest_query_id: query_id,
                text,
                parse_mode,
            },
        )
        .await
    }

    /// Retrieve access settings for a managed bot.
    ///
    /// # Errors
    ///
    /// Returns [`TelegramApiError`] on HTTP failure or when `ok: false`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_channels::telegram_api_ext::TelegramApiClient;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let client = TelegramApiClient::new("TOKEN");
    /// let settings = client.get_managed_bot_access_settings().await?;
    /// println!("private_chats={}", settings.allow_private_chats);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_managed_bot_access_settings(
        &self,
    ) -> Result<BotAccessSettings, TelegramApiError> {
        self.post("getManagedBotAccessSettings", &serde_json::json!({}))
            .await
    }

    /// Update access settings for a managed bot.
    ///
    /// Returns `true` when the settings were applied successfully.
    ///
    /// # Errors
    ///
    /// Returns [`TelegramApiError`] on HTTP failure or when `ok: false`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_channels::telegram_api_ext::{BotAccessSettings, TelegramApiClient};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let client = TelegramApiClient::new("TOKEN");
    /// let ok = client.set_managed_bot_access_settings(&BotAccessSettings {
    ///     allow_private_chats: true,
    ///     allow_group_chats: false,
    ///     allow_channel_posts: false,
    /// }).await?;
    /// assert!(ok);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn set_managed_bot_access_settings(
        &self,
        settings: &BotAccessSettings,
    ) -> Result<bool, TelegramApiError> {
        self.post("setManagedBotAccessSettings", settings).await
    }

    /// Delete a specific reaction left by `user_id` on a message.
    ///
    /// Returns `true` on success.
    ///
    /// # Arguments
    ///
    /// * `chat_id` — identifier of the chat containing the message.
    /// * `message_id` — identifier of the message.
    /// * `user_id` — identifier of the user whose reaction to remove.
    /// * `reaction` — emoji or custom reaction string to remove.
    ///
    /// # Errors
    ///
    /// Returns [`TelegramApiError`] on HTTP failure or when `ok: false`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_channels::telegram_api_ext::TelegramApiClient;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let client = TelegramApiClient::new("TOKEN");
    /// let ok = client.delete_message_reaction(123, 456, 789, "👍").await?;
    /// assert!(ok);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn delete_message_reaction(
        &self,
        chat_id: i64,
        message_id: i64,
        user_id: i64,
        reaction: &str,
    ) -> Result<bool, TelegramApiError> {
        #[derive(Serialize)]
        struct Req<'a> {
            chat_id: i64,
            message_id: i64,
            user_id: i64,
            reaction: &'a str,
        }
        self.post(
            "deleteMessageReaction",
            &Req {
                chat_id,
                message_id,
                user_id,
                reaction,
            },
        )
        .await
    }

    /// Delete all reactions left by `user_id` on a message.
    ///
    /// Returns `true` on success.
    ///
    /// # Arguments
    ///
    /// * `chat_id` — identifier of the chat containing the message.
    /// * `message_id` — identifier of the message.
    /// * `user_id` — identifier of the user whose reactions to remove.
    ///
    /// # Errors
    ///
    /// Returns [`TelegramApiError`] on HTTP failure or when `ok: false`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_channels::telegram_api_ext::TelegramApiClient;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let client = TelegramApiClient::new("TOKEN");
    /// let ok = client.delete_all_message_reactions(123, 456, 789).await?;
    /// assert!(ok);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn delete_all_message_reactions(
        &self,
        chat_id: i64,
        message_id: i64,
        user_id: i64,
    ) -> Result<bool, TelegramApiError> {
        #[derive(Serialize)]
        #[allow(clippy::struct_field_names)]
        struct Req {
            chat_id: i64,
            message_id: i64,
            user_id: i64,
        }
        self.post(
            "deleteAllMessageReactions",
            &Req {
                chat_id,
                message_id,
                user_id,
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ok_body(result: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "ok": true, "result": result })
    }

    fn err_body(description: &str) -> serde_json::Value {
        serde_json::json!({ "ok": false, "description": description })
    }

    // ── Serde round-trip tests ────────────────────────────────────────────────

    #[test]
    fn sent_guest_message_round_trip() {
        let original = SentGuestMessage {
            message_id: 42,
            chat_id: 100,
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: SentGuestMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn bot_access_settings_round_trip() {
        let original = BotAccessSettings {
            allow_private_chats: true,
            allow_group_chats: false,
            allow_channel_posts: true,
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: BotAccessSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn guest_message_round_trip() {
        let original = GuestMessage {
            message_id: 7,
            from_chat_id: 999,
            text: Some("hello".into()),
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: GuestMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn guest_message_round_trip_no_text() {
        let original = GuestMessage {
            message_id: 1,
            from_chat_id: 2,
            text: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: GuestMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    // ── answer_guest_query ────────────────────────────────────────────────────

    #[tokio::test]
    async fn answer_guest_query_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/answerGuestQuery$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&serde_json::json!({
                    "message_id": 123,
                    "chat_id": 456
                }))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let result = client
            .answer_guest_query("qid", "hello", None)
            .await
            .unwrap();
        assert_eq!(result.message_id, 123);
        assert_eq!(result.chat_id, 456);
    }

    #[tokio::test]
    async fn answer_guest_query_with_parse_mode() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/answerGuestQuery$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&serde_json::json!({
                    "message_id": 1,
                    "chat_id": 2
                }))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let result = client
            .answer_guest_query("qid", "<b>bold</b>", Some("HTML"))
            .await
            .unwrap();
        assert_eq!(result.message_id, 1);
    }

    #[tokio::test]
    async fn answer_guest_query_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/answerGuestQuery$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(err_body("Bad Request: query not found")),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let err = client
            .answer_guest_query("bad_id", "hi", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, TelegramApiError::Api(_)),
            "expected Api error"
        );
    }

    // ── get_managed_bot_access_settings ──────────────────────────────────────

    #[tokio::test]
    async fn get_managed_bot_access_settings_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getManagedBotAccessSettings$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&serde_json::json!({
                    "allow_private_chats": true,
                    "allow_group_chats": false,
                    "allow_channel_posts": true
                }))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let settings = client.get_managed_bot_access_settings().await.unwrap();
        assert!(settings.allow_private_chats);
        assert!(!settings.allow_group_chats);
        assert!(settings.allow_channel_posts);
    }

    #[tokio::test]
    async fn get_managed_bot_access_settings_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getManagedBotAccessSettings$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(err_body("Forbidden: bot is not a member")),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let err = client.get_managed_bot_access_settings().await.unwrap_err();
        assert!(matches!(err, TelegramApiError::Api(_)));
    }

    // ── set_managed_bot_access_settings ──────────────────────────────────────

    #[tokio::test]
    async fn set_managed_bot_access_settings_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/setManagedBotAccessSettings$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&serde_json::Value::Bool(true))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let ok = client
            .set_managed_bot_access_settings(&BotAccessSettings {
                allow_private_chats: true,
                allow_group_chats: true,
                allow_channel_posts: false,
            })
            .await
            .unwrap();
        assert!(ok);
    }

    #[tokio::test]
    async fn set_managed_bot_access_settings_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/setManagedBotAccessSettings$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(err_body("Bad Request: invalid settings")),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let err = client
            .set_managed_bot_access_settings(&BotAccessSettings {
                allow_private_chats: false,
                allow_group_chats: false,
                allow_channel_posts: false,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, TelegramApiError::Api(_)));
    }

    // ── delete_message_reaction ───────────────────────────────────────────────

    #[tokio::test]
    async fn delete_message_reaction_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/deleteMessageReaction$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&serde_json::Value::Bool(true))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let ok = client
            .delete_message_reaction(100, 200, 300, "👍")
            .await
            .unwrap();
        assert!(ok);
    }

    #[tokio::test]
    async fn delete_message_reaction_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/deleteMessageReaction$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(err_body("Bad Request: message not found")),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let err = client
            .delete_message_reaction(1, 2, 3, "👎")
            .await
            .unwrap_err();
        assert!(matches!(err, TelegramApiError::Api(_)));
    }

    // ── delete_all_message_reactions ─────────────────────────────────────────

    #[tokio::test]
    async fn delete_all_message_reactions_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/deleteAllMessageReactions$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&serde_json::Value::Bool(true))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let ok = client
            .delete_all_message_reactions(100, 200, 300)
            .await
            .unwrap();
        assert!(ok);
    }

    #[tokio::test]
    async fn delete_all_message_reactions_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/deleteAllMessageReactions$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(err_body("Forbidden: not enough rights")),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let err = client
            .delete_all_message_reactions(1, 2, 3)
            .await
            .unwrap_err();
        assert!(matches!(err, TelegramApiError::Api(_)));
    }

    // ── HTTP status error surfacing ───────────────────────────────────────────

    #[tokio::test]
    async fn http_429_surfaces_as_http_error_not_serde_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/answerGuestQuery$"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let err = client
            .answer_guest_query("qid", "hi", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, TelegramApiError::Http(_)),
            "expected Http error for 429, got {err:?}"
        );
    }
}
