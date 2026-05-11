// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`ReactionModerationBackend`] implementation backed by [`TelegramApiClient`].
//!
//! [`TelegramModerationBackend`] wraps a [`TelegramApiClient`] and implements the
//! `zeph_tools::ReactionModerationBackend` trait so that
//! `zeph_tools::ModerationExecutor` can call the Telegram Bot API without a direct
//! dependency on `zeph-channels`.
//!
//! [`ReactionModerationBackend`]: zeph_tools::ReactionModerationBackend
//! [`TelegramApiClient`]: crate::telegram_api_ext::TelegramApiClient

use zeph_tools::{ModerationError, ReactionModerationBackend};

use crate::telegram_api_ext::{TelegramApiClient, TelegramApiError};

/// Converts a [`TelegramApiError`] to the protocol-neutral [`ModerationError`].
fn to_moderation_error(e: TelegramApiError) -> ModerationError {
    match e {
        TelegramApiError::Api(msg) => ModerationError::Api(msg),
        TelegramApiError::Http(e) => ModerationError::Http(e.to_string()),
    }
}

/// [`ReactionModerationBackend`] implementation that calls the Telegram Bot API.
///
/// Wraps a [`TelegramApiClient`] and the bot's own Telegram user ID. Both
/// deletion methods perform a `getChatMember` pre-flight check to verify that
/// the bot is an administrator in the target chat before issuing the mutation.
/// If the check fails, [`ModerationError::Api`] is returned immediately with
/// a descriptive message rather than forwarding a `Forbidden` error from the API.
///
/// # Examples
///
/// ```no_run
/// use zeph_channels::telegram_api_ext::TelegramApiClient;
/// use zeph_channels::telegram_moderation::TelegramModerationBackend;
/// use zeph_tools::ModerationExecutor;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let api = TelegramApiClient::new("BOT_TOKEN");
/// let me = api.get_me().await?;
/// let backend = TelegramModerationBackend::new(api, me.id);
/// let executor = ModerationExecutor::new(backend);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct TelegramModerationBackend {
    client: TelegramApiClient,
    bot_user_id: i64,
}

impl TelegramModerationBackend {
    /// Create a new backend using the given API client and the bot's own user ID.
    ///
    /// `bot_user_id` is used for the `getChatMember` admin pre-check before each
    /// deletion. Obtain it by calling [`TelegramApiClient::get_me`] at startup.
    #[must_use]
    pub fn new(client: TelegramApiClient, bot_user_id: i64) -> Self {
        Self {
            client,
            bot_user_id,
        }
    }

    /// Check whether the bot (identified by `bot_user_id`) is an administrator
    /// in `chat_id`.
    ///
    /// Returns `Ok(true)` when the bot is a creator or administrator, `Ok(false)`
    /// when it is a regular member, and `Err` on API or transport failure.
    ///
    /// # Errors
    ///
    /// Returns [`ModerationError`] on API or transport failure.
    pub async fn bot_is_admin(
        &self,
        chat_id: i64,
        bot_user_id: i64,
    ) -> Result<bool, ModerationError> {
        let member = self
            .client
            .get_chat_member(chat_id, bot_user_id)
            .await
            .map_err(to_moderation_error)?;
        Ok(member.is_admin())
    }
}

impl ReactionModerationBackend for TelegramModerationBackend {
    fn delete_reaction<'a>(
        &'a self,
        chat_id: i64,
        message_id: i64,
        user_id: i64,
        reaction: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ModerationError>> + Send + 'a>>
    {
        Box::pin(async move {
            if !self.bot_is_admin(chat_id, self.bot_user_id).await? {
                return Err(ModerationError::Api(
                    "bot is not an administrator in this chat".into(),
                ));
            }
            self.client
                .delete_message_reaction(chat_id, message_id, user_id, reaction)
                .await
                .map_err(to_moderation_error)?;
            Ok(())
        })
    }

    fn delete_all_reactions<'a>(
        &'a self,
        chat_id: i64,
        message_id: i64,
        user_id: i64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ModerationError>> + Send + 'a>>
    {
        Box::pin(async move {
            if !self.bot_is_admin(chat_id, self.bot_user_id).await? {
                return Err(ModerationError::Api(
                    "bot is not an administrator in this chat".into(),
                ));
            }
            self.client
                .delete_all_message_reactions(chat_id, message_id, user_id)
                .await
                .map_err(to_moderation_error)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const BOT_ID: i64 = 42;

    fn ok_body(result: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "ok": true, "result": result })
    }

    fn err_body(description: &str) -> serde_json::Value {
        serde_json::json!({ "ok": false, "description": description })
    }

    fn admin_member_body() -> serde_json::Value {
        serde_json::json!({
            "status": "administrator",
            "user": { "id": BOT_ID, "is_bot": true, "first_name": "MyBot" }
        })
    }

    fn non_admin_member_body() -> serde_json::Value {
        serde_json::json!({
            "status": "member",
            "user": { "id": BOT_ID, "is_bot": true, "first_name": "MyBot" }
        })
    }

    // ── delete_reaction ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_reaction_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getChatMember$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(&admin_member_body())))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(".*/deleteMessageReaction$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&serde_json::Value::Bool(true))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let backend = TelegramModerationBackend::new(client, BOT_ID);
        backend.delete_reaction(1, 2, 3, "👍").await.unwrap();
    }

    #[tokio::test]
    async fn delete_reaction_rejected_when_not_admin() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getChatMember$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&non_admin_member_body())),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let backend = TelegramModerationBackend::new(client, BOT_ID);
        let err = backend.delete_reaction(1, 2, 3, "👍").await.unwrap_err();
        assert!(
            matches!(err, ModerationError::Api(ref msg) if msg.contains("not an administrator")),
            "expected admin check error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn delete_reaction_api_error_surfaces_as_moderation_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getChatMember$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(&admin_member_body())))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(".*/deleteMessageReaction$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(err_body("Bad Request: message not found")),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let backend = TelegramModerationBackend::new(client, BOT_ID);
        let err = backend.delete_reaction(1, 2, 3, "👎").await.unwrap_err();
        assert!(
            matches!(err, ModerationError::Api(_)),
            "expected Api error, got {err:?}"
        );
    }

    // ── delete_all_reactions ──────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_all_reactions_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getChatMember$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(&admin_member_body())))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(".*/deleteAllMessageReactions$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&serde_json::Value::Bool(true))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let backend = TelegramModerationBackend::new(client, BOT_ID);
        backend.delete_all_reactions(10, 20, 30).await.unwrap();
    }

    #[tokio::test]
    async fn delete_all_reactions_rejected_when_not_admin() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getChatMember$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&non_admin_member_body())),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let backend = TelegramModerationBackend::new(client, BOT_ID);
        let err = backend.delete_all_reactions(1, 2, 3).await.unwrap_err();
        assert!(
            matches!(err, ModerationError::Api(ref msg) if msg.contains("not an administrator")),
            "expected admin check error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn delete_all_reactions_api_error_after_admin_check() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getChatMember$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body(&admin_member_body())))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(".*/deleteAllMessageReactions$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(err_body("Forbidden: not enough rights")),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let backend = TelegramModerationBackend::new(client, BOT_ID);
        let err = backend.delete_all_reactions(1, 2, 3).await.unwrap_err();
        assert!(matches!(err, ModerationError::Api(_)));
    }

    // ── bot_is_admin ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn bot_is_admin_administrator_returns_true() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getChatMember$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&serde_json::json!({
                    "status": "administrator",
                    "user": { "id": BOT_ID, "is_bot": true, "first_name": "MyBot" }
                }))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let backend = TelegramModerationBackend::new(client, BOT_ID);
        assert!(backend.bot_is_admin(100, BOT_ID).await.unwrap());
    }

    #[tokio::test]
    async fn bot_is_admin_member_returns_false() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getChatMember$"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_body(&serde_json::json!({
                    "status": "member",
                    "user": { "id": BOT_ID, "is_bot": true, "first_name": "MyBot" }
                }))),
            )
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let backend = TelegramModerationBackend::new(client, BOT_ID);
        assert!(!backend.bot_is_admin(100, BOT_ID).await.unwrap());
    }

    // ── HTTP error maps to ModerationError::Http ──────────────────────────────

    #[tokio::test]
    async fn delete_reaction_http_error_on_admin_check_surfaces_as_moderation_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(".*/getChatMember$"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
            .mount(&server)
            .await;

        let client = TelegramApiClient::with_base_url(server.uri());
        let backend = TelegramModerationBackend::new(client, BOT_ID);
        let err = backend.delete_reaction(1, 2, 3, "👍").await.unwrap_err();
        assert!(
            matches!(err, ModerationError::Http(_)),
            "expected Http error, got {err:?}"
        );
    }
}
