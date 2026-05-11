// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reaction moderation executor for Telegram Bot API 10.0.
//!
//! Exposes two structured tool calls — `telegram_delete_reaction` and
//! `telegram_delete_all_reactions` — that let the agent remove emoji reactions
//! from messages in chats where the bot has admin rights.
//!
//! The executor is platform-agnostic: it delegates the actual API calls to
//! a [`ReactionModerationBackend`] implementation, keeping `zeph-tools`
//! independent of `zeph-channels`.
//!
//! # Wiring
//!
//! In `src/agent_setup.rs`, build a `TelegramModerationBackend` (from
//! `zeph-channels`) and wrap it with [`ModerationExecutor`]:
//!
//! ```ignore
//! use zeph_channels::telegram_moderation::TelegramModerationBackend;
//! use zeph_tools::moderation::ModerationExecutor;
//!
//! let api = telegram_channel.api_ext().clone();
//! let me = api.get_me().await?;
//! let backend = TelegramModerationBackend::new(api, me.id);
//! let executor = ModerationExecutor::new(backend);
//! ```

use schemars::JsonSchema;
use serde::Deserialize;
use zeph_common::ToolName;

use crate::executor::{
    ClaimSource, ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params,
};
use crate::registry::{InvocationHint, ToolDef};

// ── Tool parameter schemas ─────────────────────────────────────────────────

/// Parameters for `telegram_delete_reaction`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteReactionParams {
    /// Telegram chat identifier (numeric).
    pub chat_id: i64,
    /// Identifier of the message whose reaction should be removed.
    pub message_id: i64,
    /// Telegram user identifier whose reaction to remove.
    pub user_id: i64,
    /// Emoji or custom reaction string to remove (e.g. `"👍"`).
    pub reaction: String,
}

/// Parameters for `telegram_delete_all_reactions`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteAllReactionsParams {
    /// Telegram chat identifier (numeric).
    pub chat_id: i64,
    /// Identifier of the message whose reactions should be cleared.
    pub message_id: i64,
    /// Telegram user identifier whose reactions to remove.
    pub user_id: i64,
}

// ── Backend trait ──────────────────────────────────────────────────────────

/// Errors produced by a [`ReactionModerationBackend`].
#[derive(Debug, thiserror::Error)]
pub enum ModerationError {
    /// The Telegram API returned an error response (`ok: false`).
    ///
    /// The description is forwarded from the API and maps to
    /// [`ToolError::InvalidParams`] so the agent can adjust its call.
    #[error("Telegram API error: {0}")]
    Api(String),
    /// HTTP transport or TLS error.
    ///
    /// Maps to a transient [`ToolError::Http`] so the agent may retry.
    #[error("HTTP error: {0}")]
    Http(String),
}

/// Backend that executes reaction-moderation API calls.
///
/// Implementors are expected to call the Telegram Bot API. The trait is
/// object-safe (all methods return pinned boxed futures) so [`ModerationExecutor`]
/// can hold it as `Arc<dyn ReactionModerationBackend>`.
///
/// # Contract
///
/// - `delete_reaction` and `delete_all_reactions` must call the Telegram API and
///   surface both `ok: false` responses as [`ModerationError::Api`] and transport
///   failures as [`ModerationError::Http`].
/// - The bot must be an administrator with appropriate rights in the target chat
///   **before** calling these methods; implementations SHOULD perform a pre-flight
///   `get_chat_member` check and return [`ModerationError::Api`] when the bot is
///   not an administrator, rather than forwarding a `Forbidden` error from the API.
pub trait ReactionModerationBackend: Send + Sync {
    /// Remove a single reaction left by `user_id` on a message.
    ///
    /// # Errors
    ///
    /// Returns [`ModerationError`] on API or transport failure.
    fn delete_reaction<'a>(
        &'a self,
        chat_id: i64,
        message_id: i64,
        user_id: i64,
        reaction: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ModerationError>> + Send + 'a>>;

    /// Remove all reactions left by `user_id` on a message.
    ///
    /// # Errors
    ///
    /// Returns [`ModerationError`] on API or transport failure.
    fn delete_all_reactions<'a>(
        &'a self,
        chat_id: i64,
        message_id: i64,
        user_id: i64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ModerationError>> + Send + 'a>>;
}

// ── Executor ───────────────────────────────────────────────────────────────

/// Tool executor for Telegram reaction moderation.
///
/// Dispatches the structured tool calls `telegram_delete_reaction` and
/// `telegram_delete_all_reactions` to the injected [`ReactionModerationBackend`].
///
/// Deleting reactions is irreversible — the executor signals
/// `requires_confirmation = true` so the user can approve before execution.
///
/// # Examples
///
/// ```no_run
/// # use zeph_tools::moderation::{ModerationExecutor, ReactionModerationBackend, ModerationError};
/// # use std::pin::Pin;
/// #
/// # struct MockBackend;
/// # impl ReactionModerationBackend for MockBackend {
/// #     fn delete_reaction<'a>(&'a self, _: i64, _: i64, _: i64, _: &'a str)
/// #         -> Pin<Box<dyn std::future::Future<Output = Result<(), ModerationError>> + Send + 'a>>
/// #     { Box::pin(async { Ok(()) }) }
/// #     fn delete_all_reactions<'a>(&'a self, _: i64, _: i64, _: i64)
/// #         -> Pin<Box<dyn std::future::Future<Output = Result<(), ModerationError>> + Send + 'a>>
/// #     { Box::pin(async { Ok(()) }) }
/// # }
/// #
/// let executor = ModerationExecutor::new(MockBackend);
/// ```
#[derive(Debug)]
pub struct ModerationExecutor<B> {
    backend: B,
}

impl<B: ReactionModerationBackend> ModerationExecutor<B> {
    /// Create a new executor backed by `backend`.
    pub fn new(backend: B) -> Self {
        Self { backend }
    }
}

/// Map a [`ModerationError`] to the appropriate [`ToolError`].
///
/// `Api` errors — e.g. `"MESSAGE_NOT_FOUND"`, `"REACTION_INVALID"` — map to
/// [`ToolError::InvalidParams`] because the call parameters were wrong, not a network issue.
/// `Http` transport errors map to [`ToolError::Http`] with status `502` (Bad Gateway) to signal
/// a transient upstream failure consistent with how other executors map network errors.
fn moderation_error_to_tool_error(e: ModerationError) -> ToolError {
    match e {
        ModerationError::Api(msg) => ToolError::InvalidParams { message: msg },
        ModerationError::Http(msg) => ToolError::Http {
            status: 502,
            message: msg,
        },
    }
}

impl<B: ReactionModerationBackend + std::fmt::Debug> ToolExecutor for ModerationExecutor<B> {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    #[tracing::instrument(skip(self), fields(tool_id = %call.tool_id))]
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        match call.tool_id.as_ref() {
            "telegram_delete_reaction" => {
                let p: DeleteReactionParams = deserialize_params(&call.params)?;
                if p.reaction.is_empty() {
                    return Err(ToolError::InvalidParams {
                        message: "reaction must not be empty".into(),
                    });
                }
                if p.reaction.chars().count() > 10 {
                    return Err(ToolError::InvalidParams {
                        message: "reaction string too long".into(),
                    });
                }
                tracing::info!(
                    chat_id = p.chat_id,
                    message_id = p.message_id,
                    user_id = p.user_id,
                    reaction = %p.reaction,
                    "moderation: deleting single reaction"
                );
                self.backend
                    .delete_reaction(p.chat_id, p.message_id, p.user_id, &p.reaction)
                    .await
                    .map_err(moderation_error_to_tool_error)?;
                Ok(Some(ToolOutput {
                    tool_name: ToolName::new("telegram_delete_reaction"),
                    summary: format!(
                        "Reaction '{}' removed from message {} in chat {} for user {}.",
                        p.reaction, p.message_id, p.chat_id, p.user_id
                    ),
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                    claim_source: Some(ClaimSource::Moderation),
                }))
            }
            "telegram_delete_all_reactions" => {
                let p: DeleteAllReactionsParams = deserialize_params(&call.params)?;
                tracing::info!(
                    chat_id = p.chat_id,
                    message_id = p.message_id,
                    user_id = p.user_id,
                    "moderation: deleting all reactions"
                );
                self.backend
                    .delete_all_reactions(p.chat_id, p.message_id, p.user_id)
                    .await
                    .map_err(moderation_error_to_tool_error)?;
                Ok(Some(ToolOutput {
                    tool_name: ToolName::new("telegram_delete_all_reactions"),
                    summary: format!(
                        "All reactions removed from message {} in chat {} for user {}.",
                        p.message_id, p.chat_id, p.user_id
                    ),
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                    claim_source: Some(ClaimSource::Moderation),
                }))
            }
            _ => Ok(None),
        }
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                id: "telegram_delete_reaction".into(),
                description: "Remove a specific emoji reaction left by a user on a Telegram message.\n\
                    Requires the bot to be an administrator with 'delete_messages' rights in the chat.\n\
                    This action is irreversible.\n\
                    Parameters: chat_id (integer, required) — chat containing the message;\n\
                      message_id (integer, required) — the target message;\n\
                      user_id (integer, required) — the user whose reaction to remove;\n\
                      reaction (string, required) — the emoji to remove (e.g. \"👍\").\n\
                    Returns: confirmation message on success.\n\
                    Errors: InvalidParams when the API returns ok=false; Http on transport failure.".into(),
                schema: schemars::schema_for!(DeleteReactionParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
            ToolDef {
                id: "telegram_delete_all_reactions".into(),
                description: "Remove all emoji reactions left by a user on a Telegram message.\n\
                    Requires the bot to be an administrator with 'delete_messages' rights in the chat.\n\
                    This action is irreversible.\n\
                    Parameters: chat_id (integer, required) — chat containing the message;\n\
                      message_id (integer, required) — the target message;\n\
                      user_id (integer, required) — the user whose reactions to remove.\n\
                    Returns: confirmation message on success.\n\
                    Errors: InvalidParams when the API returns ok=false; Http on transport failure.".into(),
                schema: schemars::schema_for!(DeleteAllReactionsParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
        ]
    }

    /// Reaction deletion is irreversible — always require confirmation.
    fn requires_confirmation(&self, call: &ToolCall) -> bool {
        matches!(
            call.tool_id.as_ref(),
            "telegram_delete_reaction" | "telegram_delete_all_reactions"
        )
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    // ── Mock backend ───────────────────────────────────────────────────────

    struct MockBackend {
        delete_calls: Arc<AtomicU32>,
        delete_all_calls: Arc<AtomicU32>,
        /// When set to `true`, all calls return `ModerationError::Api`.
        fail: bool,
    }

    impl MockBackend {
        fn new(fail: bool) -> (Self, Arc<AtomicU32>, Arc<AtomicU32>) {
            let d = Arc::new(AtomicU32::new(0));
            let da = Arc::new(AtomicU32::new(0));
            (
                Self {
                    delete_calls: Arc::clone(&d),
                    delete_all_calls: Arc::clone(&da),
                    fail,
                },
                d,
                da,
            )
        }
    }

    impl std::fmt::Debug for MockBackend {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MockBackend").finish_non_exhaustive()
        }
    }

    impl ReactionModerationBackend for MockBackend {
        fn delete_reaction<'a>(
            &'a self,
            _chat_id: i64,
            _message_id: i64,
            _user_id: i64,
            _reaction: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), ModerationError>> + Send + 'a>,
        > {
            let fail = self.fail;
            let counter = Arc::clone(&self.delete_calls);
            Box::pin(async move {
                if fail {
                    Err(ModerationError::Api(
                        "Bad Request: message not found".into(),
                    ))
                } else {
                    counter.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            })
        }

        fn delete_all_reactions<'a>(
            &'a self,
            _chat_id: i64,
            _message_id: i64,
            _user_id: i64,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), ModerationError>> + Send + 'a>,
        > {
            let fail = self.fail;
            let counter = Arc::clone(&self.delete_all_calls);
            Box::pin(async move {
                if fail {
                    Err(ModerationError::Api("Forbidden: not enough rights".into()))
                } else {
                    counter.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            })
        }
    }

    fn make_call(tool_id: &str, params: &serde_json::Value) -> ToolCall {
        ToolCall {
            tool_id: ToolName::new(tool_id),
            params: params.as_object().cloned().unwrap_or_default(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
        }
    }

    // ── execute returns None for unknown tool ──────────────────────────────

    #[tokio::test]
    async fn unknown_tool_returns_none() {
        let (backend, _, _) = MockBackend::new(false);
        let exec = ModerationExecutor::new(backend);
        let call = make_call("unknown_tool", &serde_json::json!({}));
        let result = exec.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn execute_fenced_returns_none() {
        let (backend, _, _) = MockBackend::new(false);
        let exec = ModerationExecutor::new(backend);
        let result = exec.execute("```bash\necho hi\n```").await.unwrap();
        assert!(result.is_none());
    }

    // ── delete_reaction success ────────────────────────────────────────────

    #[tokio::test]
    async fn delete_reaction_success() {
        let (backend, d_calls, _) = MockBackend::new(false);
        let exec = ModerationExecutor::new(backend);
        let call = make_call(
            "telegram_delete_reaction",
            &serde_json::json!({
                "chat_id": 100,
                "message_id": 200,
                "user_id": 300,
                "reaction": "👍"
            }),
        );
        let output = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert_eq!(output.tool_name.as_ref(), "telegram_delete_reaction");
        assert!(output.summary.contains("👍"));
        assert!(output.summary.contains("200"));
        assert_eq!(d_calls.load(Ordering::Relaxed), 1);
        assert_eq!(output.claim_source, Some(ClaimSource::Moderation));
    }

    // ── delete_all_reactions success ───────────────────────────────────────

    #[tokio::test]
    async fn delete_all_reactions_success() {
        let (backend, _, da_calls) = MockBackend::new(false);
        let exec = ModerationExecutor::new(backend);
        let call = make_call(
            "telegram_delete_all_reactions",
            &serde_json::json!({
                "chat_id": 100,
                "message_id": 200,
                "user_id": 300
            }),
        );
        let output = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert_eq!(output.tool_name.as_ref(), "telegram_delete_all_reactions");
        assert!(output.summary.contains("All reactions removed"));
        assert_eq!(da_calls.load(Ordering::Relaxed), 1);
    }

    // ── API error maps to InvalidParams ───────────────────────────────────

    #[tokio::test]
    async fn delete_reaction_api_error_maps_to_invalid_params() {
        let (backend, _, _) = MockBackend::new(true);
        let exec = ModerationExecutor::new(backend);
        let call = make_call(
            "telegram_delete_reaction",
            &serde_json::json!({
                "chat_id": 1,
                "message_id": 2,
                "user_id": 3,
                "reaction": "👎"
            }),
        );
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams { .. }),
            "expected InvalidParams, got {err:?}"
        );
    }

    #[tokio::test]
    async fn delete_all_reactions_api_error_maps_to_invalid_params() {
        let (backend, _, _) = MockBackend::new(true);
        let exec = ModerationExecutor::new(backend);
        let call = make_call(
            "telegram_delete_all_reactions",
            &serde_json::json!({
                "chat_id": 1,
                "message_id": 2,
                "user_id": 3
            }),
        );
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams { .. }),
            "expected InvalidParams, got {err:?}"
        );
    }

    // ── Invalid params ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_reaction_missing_params_returns_invalid_params() {
        let (backend, _, _) = MockBackend::new(false);
        let exec = ModerationExecutor::new(backend);
        // reaction field missing
        let call = make_call(
            "telegram_delete_reaction",
            &serde_json::json!({
                "chat_id": 1,
                "message_id": 2,
                "user_id": 3
            }),
        );
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    #[tokio::test]
    async fn delete_all_reactions_missing_params_returns_invalid_params() {
        let (backend, _, _) = MockBackend::new(false);
        let exec = ModerationExecutor::new(backend);
        // user_id field missing
        let call = make_call(
            "telegram_delete_all_reactions",
            &serde_json::json!({
                "chat_id": 1,
                "message_id": 2
            }),
        );
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    // ── requires_confirmation ─────────────────────────────────────────────

    #[test]
    fn requires_confirmation_for_delete_reaction() {
        let (backend, _, _) = MockBackend::new(false);
        let exec = ModerationExecutor::new(backend);
        let call = make_call(
            "telegram_delete_reaction",
            &serde_json::json!({
                "chat_id": 1, "message_id": 2, "user_id": 3, "reaction": "👍"
            }),
        );
        assert!(exec.requires_confirmation(&call));
    }

    #[test]
    fn requires_confirmation_for_delete_all_reactions() {
        let (backend, _, _) = MockBackend::new(false);
        let exec = ModerationExecutor::new(backend);
        let call = make_call(
            "telegram_delete_all_reactions",
            &serde_json::json!({
                "chat_id": 1, "message_id": 2, "user_id": 3
            }),
        );
        assert!(exec.requires_confirmation(&call));
    }

    #[test]
    fn does_not_require_confirmation_for_unknown_tool() {
        let (backend, _, _) = MockBackend::new(false);
        let exec = ModerationExecutor::new(backend);
        let call = make_call("unknown", &serde_json::json!({}));
        assert!(!exec.requires_confirmation(&call));
    }

    // ── tool_definitions ──────────────────────────────────────────────────

    #[test]
    fn tool_definitions_returns_two_tools() {
        let (backend, _, _) = MockBackend::new(false);
        let exec = ModerationExecutor::new(backend);
        let defs = exec.tool_definitions();
        assert_eq!(defs.len(), 2);
        let ids: Vec<&str> = defs.iter().map(|d| d.id.as_ref()).collect();
        assert!(ids.contains(&"telegram_delete_reaction"));
        assert!(ids.contains(&"telegram_delete_all_reactions"));
    }

    // ── Http error maps correctly ─────────────────────────────────────────

    #[test]
    fn moderation_error_http_maps_to_tool_error_http_502() {
        let err = ModerationError::Http("connection refused".into());
        let te = moderation_error_to_tool_error(err);
        assert!(matches!(te, ToolError::Http { status: 502, .. }));
    }

    // ── reaction validation ────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_reaction_empty_reaction_returns_invalid_params() {
        let (backend, _, _) = MockBackend::new(false);
        let exec = ModerationExecutor::new(backend);
        let call = make_call(
            "telegram_delete_reaction",
            &serde_json::json!({
                "chat_id": 1,
                "message_id": 2,
                "user_id": 3,
                "reaction": ""
            }),
        );
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams { ref message } if message.contains("empty")),
            "expected empty reaction error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn delete_reaction_overlong_reaction_returns_invalid_params() {
        let (backend, _, _) = MockBackend::new(false);
        let exec = ModerationExecutor::new(backend);
        let call = make_call(
            "telegram_delete_reaction",
            &serde_json::json!({
                "chat_id": 1,
                "message_id": 2,
                "user_id": 3,
                "reaction": "12345678901"  // 11 chars — exceeds limit of 10
            }),
        );
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams { ref message } if message.contains("too long")),
            "expected too long error, got {err:?}"
        );
    }
}
