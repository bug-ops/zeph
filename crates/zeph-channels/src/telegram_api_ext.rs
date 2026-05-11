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

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Per-request timeout applied to every `reqwest::Client` created by
/// [`TelegramApiClient`]. Matches the project's general policy for external
/// HTTP calls that are not long-polling or streaming.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Response payload from the `answerGuestQuery` Bot API method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SentGuestMessage {
    /// Telegram message identifier of the sent reply.
    pub message_id: i64,
    /// Identifier of the chat the message was sent to.
    pub chat_id: i64,
}

/// Access settings for a managed bot in Guest Mode (Bot API 10.0).
///
/// Controls which message sources the managed bot is allowed to receive.
/// Both fields default to `false`; set `allow_bot_messages = true` to enable
/// bot-to-bot communication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BotAccessSettings {
    /// Whether the managed bot may receive messages from users.
    pub allow_user_messages: bool,
    /// Whether the managed bot may receive messages from other bots.
    pub allow_bot_messages: bool,
}

/// Minimal user info extracted from a guest message update.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GuestUser {
    /// Telegram user identifier.
    pub id: i64,
    /// Whether this user is a bot.
    pub is_bot: bool,
    /// Telegram username, without `@`.
    pub username: Option<String>,
    /// Display name.
    pub first_name: String,
}

/// Minimal chat info extracted from a guest message update.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GuestChat {
    /// Telegram chat identifier.
    pub id: i64,
    /// Chat type (e.g. `"group"`, `"supergroup"`, `"channel"`).
    #[serde(rename = "type")]
    pub chat_type: String,
}

/// A guest message update from Bot API 10.0.
///
/// Received when a user @mentions the bot in a chat where the bot is not a
/// member. The `guest_query_id` is required for responding via
/// [`TelegramApiClient::answer_guest_query`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GuestMessage {
    /// Opaque identifier for this guest query, used in `answerGuestQuery`.
    pub guest_query_id: String,
    /// The user who @mentioned the bot.
    pub guest_bot_caller_user: GuestUser,
    /// The chat where the mention occurred.
    pub guest_bot_caller_chat: GuestChat,
    /// Text content of the message, if any.
    pub text: Option<String>,
}

/// Chat member status as returned by `getChatMember`.
///
/// Only the status variants relevant to admin checks are represented. Any
/// unrecognised status string is captured by the `Other` variant.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatMemberStatus {
    /// The user is the chat creator.
    Creator,
    /// The user is an administrator.
    Administrator,
    /// The user is a regular member.
    Member,
    /// The user is restricted.
    Restricted,
    /// The user has left the chat.
    Left,
    /// The user was kicked or banned.
    Kicked,
    /// Unknown or future status value.
    #[serde(other)]
    Other,
}

/// Minimal chat member info returned by `getChatMember`.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatMember {
    /// Membership status.
    pub status: ChatMemberStatus,
    /// The user this record describes.
    pub user: GuestUser,
}

impl ChatMember {
    /// Whether this member has admin-level privileges (creator or administrator).
    #[must_use]
    pub fn is_admin(&self) -> bool {
        matches!(
            self.status,
            ChatMemberStatus::Creator | ChatMemberStatus::Administrator
        )
    }
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
#[derive(Clone)]
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
    /// Creates an independent `reqwest::Client` with its own connection pool and
    /// a [`REQUEST_TIMEOUT`] per-request timeout. To share a connection pool with
    /// an existing client, use [`TelegramApiClient::with_client`].
    ///
    /// # Panics
    ///
    /// Panics if the TLS backend cannot be initialised (i.e. `reqwest::ClientBuilder::build`
    /// returns an error). This does not occur in practice when the crate is compiled with a
    /// supported TLS backend.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        let token = token.into();
        Self {
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("reqwest TLS backend unavailable"),
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

    /// Fetch the bot's own user information via `getMe`.
    ///
    /// Returns the bot's Telegram user ID. This is the value to pass as
    /// `bot_user_id` when constructing a `TelegramModerationBackend` for
    /// pre-flight admin checks.
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
    /// let me = client.get_me().await?;
    /// println!("bot user id: {}", me.id);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_me(&self) -> Result<GuestUser, TelegramApiError> {
        self.post("getMe", &serde_json::json!({})).await
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
    /// # Panics
    ///
    /// Panics if the TLS backend cannot be initialised. This does not occur in practice
    /// when the crate is compiled with a supported TLS backend.
    ///
    /// [`new`]: TelegramApiClient::new
    #[must_use]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("reqwest TLS backend unavailable"),
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
    /// println!("bot_messages={}", settings.allow_bot_messages);
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
    ///     allow_user_messages: true,
    ///     allow_bot_messages: true,
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

    /// Retrieve the membership status of `user_id` in `chat_id`.
    ///
    /// Used for a pre-flight admin check before executing moderation actions. The
    /// result is not cached — each call makes a live API request.
    ///
    /// # Arguments
    ///
    /// * `chat_id` — identifier of the chat to query.
    /// * `user_id` — identifier of the user whose membership to retrieve.
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
    /// let member = client.get_chat_member(123, 456).await?;
    /// println!("is admin: {}", member.is_admin());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_chat_member(
        &self,
        chat_id: i64,
        user_id: i64,
    ) -> Result<ChatMember, TelegramApiError> {
        #[derive(Serialize)]
        struct Req {
            chat_id: i64,
            user_id: i64,
        }
        self.post("getChatMember", &Req { chat_id, user_id }).await
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
            allow_user_messages: true,
            allow_bot_messages: false,
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: BotAccessSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn guest_message_round_trip() {
        let original = GuestMessage {
            guest_query_id: "qid_abc".into(),
            guest_bot_caller_user: GuestUser {
                id: 123,
                is_bot: false,
                username: Some("alice".into()),
                first_name: "Alice".into(),
            },
            guest_bot_caller_chat: GuestChat {
                id: 999,
                chat_type: "group".into(),
            },
            text: Some("hello".into()),
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: GuestMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn guest_message_round_trip_no_text() {
        let original = GuestMessage {
            guest_query_id: "qid_def".into(),
            guest_bot_caller_user: GuestUser {
                id: 1,
                is_bot: false,
                username: None,
                first_name: "Bob".into(),
            },
            guest_bot_caller_chat: GuestChat {
                id: 2,
                chat_type: "supergroup".into(),
            },
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
                    "allow_user_messages": true,
                    "allow_bot_messages": false
                }))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let settings = client.get_managed_bot_access_settings().await.unwrap();
        assert!(settings.allow_user_messages);
        assert!(!settings.allow_bot_messages);
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
                allow_user_messages: true,
                allow_bot_messages: true,
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
                allow_user_messages: false,
                allow_bot_messages: false,
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

    // ── get_chat_member ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_chat_member_administrator_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getChatMember$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&serde_json::json!({
                    "status": "administrator",
                    "user": {
                        "id": 456,
                        "is_bot": false,
                        "first_name": "Alice",
                        "username": "alice"
                    }
                }))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let member = client.get_chat_member(123, 456).await.unwrap();
        assert!(member.is_admin());
        assert_eq!(member.user.id, 456);
    }

    #[tokio::test]
    async fn get_chat_member_creator_is_admin() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getChatMember$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&serde_json::json!({
                    "status": "creator",
                    "user": {
                        "id": 1,
                        "is_bot": false,
                        "first_name": "Owner"
                    }
                }))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let member = client.get_chat_member(100, 1).await.unwrap();
        assert!(member.is_admin());
    }

    #[tokio::test]
    async fn get_chat_member_regular_member_is_not_admin() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getChatMember$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&serde_json::json!({
                    "status": "member",
                    "user": {
                        "id": 99,
                        "is_bot": false,
                        "first_name": "Bob"
                    }
                }))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let member = client.get_chat_member(100, 99).await.unwrap();
        assert!(!member.is_admin());
    }

    #[tokio::test]
    async fn get_chat_member_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getChatMember$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(err_body("Bad Request: user not found")),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let err = client.get_chat_member(1, 2).await.unwrap_err();
        assert!(matches!(err, TelegramApiError::Api(_)));
    }

    // ── get_me ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_me_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getMe$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&serde_json::json!({
                    "id": 123_456,
                    "is_bot": true,
                    "first_name": "MyBot",
                    "username": "my_bot"
                }))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let me = client.get_me().await.unwrap();
        assert_eq!(me.id, 123_456);
        assert!(me.is_bot);
    }

    // ── Timeout enforcement ───────────────────────────────────────────────────

    #[tokio::test]
    async fn request_times_out_when_server_is_slow() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getMe$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(300))
                    .set_body_json(ok_body(&serde_json::json!({
                        "id": 1, "is_bot": true, "first_name": "Bot"
                    }))),
            )
            .mount(&server)
            .await;

        // Use with_client() to inject a short timeout so the test stays fast.
        let short_timeout_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(50))
            .build()
            .unwrap();
        let client = TelegramApiClient::with_client(short_timeout_client, "TOKEN");
        // Override base_url to point at the mock server.
        let mut client = client;
        client.base_url = server.uri();

        let err = client.get_me().await.unwrap_err();
        assert!(
            matches!(err, TelegramApiError::Http(_)),
            "expected Http (timeout) error, got {err:?}"
        );
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
