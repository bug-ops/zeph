// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Discord channel adapter using Gateway WebSocket + REST API.

pub mod gateway;
pub mod rest;

use std::time::Duration;

use tokio::sync::mpsc;
use zeph_common::TaskSupervisor;
use zeph_core::channel::{
    Channel, ChannelError, ChannelMessage, ElicitationRequest, ElicitationResponse,
};

use self::gateway::IncomingMessage;
use crate::streaming::{StreamingBuffer, StreamingSend};

const MAX_MESSAGE_LEN: usize = 2000;
const EDIT_THROTTLE: Duration = Duration::from_millis(1500);

/// Discord channel adapter implementing edit-in-place streaming.
pub struct DiscordChannel {
    rx: mpsc::Receiver<IncomingMessage>,
    rest: rest::RestClient,
    /// Gateway WebSocket listener handle. `None` when the gateway is supervised by
    /// a [`TaskSupervisor`] (lifecycle is owned by the supervisor in that case).
    _gateway_handle: Option<tokio::task::JoinHandle<()>>,
    channel_id: Option<String>,
    allowed_user_ids: Vec<String>,
    allowed_role_ids: Vec<String>,
    allowed_channel_ids: Vec<String>,
    buffer: StreamingBuffer,
    message_id: Option<String>,
    /// Optional supervisor used to register discord tasks in the workspace-wide task
    /// registry with automatic restart on panic and lifecycle observability.
    supervisor: Option<TaskSupervisor>,
}

impl std::fmt::Debug for DiscordChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordChannel")
            .field("channel_id", &self.channel_id)
            .field("supervisor", &self.supervisor.is_some())
            .finish_non_exhaustive()
    }
}

impl DiscordChannel {
    /// Create a new Discord channel and spawn the gateway listener.
    ///
    /// Slash commands are registered at startup in a supervised fire-and-forget task.
    /// If registration fails, a warning is logged and the bot continues normally.
    ///
    /// When `supervisor` is provided the gateway and slash-command tasks are registered
    /// in the workspace-wide task registry with automatic restart on panic and lifecycle
    /// observability. Without a supervisor both tasks fall back to plain `tokio::spawn`
    /// with a warning — acceptable in tests but not recommended for production.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelError::Other`] when `allowed_user_ids` and
    /// `allowed_role_ids` are both empty. An unconfigured allowlist is refused
    /// rather than treated as "allow everyone" — mirrors the Telegram channel's
    /// fail-closed startup check (see [`crate::auth::require_configured_allowlist`]).
    /// The check runs before any gateway connection or slash-command registration,
    /// so a misconfigured adapter has no observable side effect.
    pub fn new(
        token: String,
        allowed_user_ids: Vec<String>,
        allowed_role_ids: Vec<String>,
        allowed_channel_ids: Vec<String>,
        supervisor: Option<&TaskSupervisor>,
    ) -> Result<Self, ChannelError> {
        crate::auth::require_configured_allowlist(
            "discord",
            &[&allowed_user_ids, &allowed_role_ids],
        )?;
        let rest = rest::RestClient::new(token.clone());
        let (gateway_handle, rx) = gateway::spawn_gateway(token, supervisor);
        Self::register_slash_commands(rest.clone(), supervisor);
        Ok(Self {
            rx,
            rest,
            _gateway_handle: gateway_handle,
            channel_id: None,
            allowed_user_ids,
            allowed_role_ids,
            allowed_channel_ids,
            buffer: StreamingBuffer::new(EDIT_THROTTLE),
            message_id: None,
            supervisor: supervisor.cloned(),
        })
    }

    /// Register Discord slash commands in a supervised fire-and-forget task.
    ///
    /// When a supervisor is provided the task is registered as `"discord_register_commands"`
    /// so it is visible in TUI status. Without a supervisor falls back to plain `tokio::spawn`.
    fn register_slash_commands(rest: rest::RestClient, supervisor: Option<&TaskSupervisor>) {
        let factory = move || {
            let rest = rest.clone();
            async move {
                rest.register_slash_commands().await;
            }
        };
        if let Some(sup) = supervisor {
            sup.spawn(zeph_common::TaskDescriptor {
                name: "discord_register_commands",
                restart: zeph_common::RestartPolicy::RunOnce,
                factory,
            });
        } else {
            tokio::spawn(factory());
        }
    }

    fn is_authorized(&self, msg: &IncomingMessage) -> bool {
        if !self.allowed_channel_ids.is_empty()
            && !self.allowed_channel_ids.contains(&msg.channel_id)
        {
            return false;
        }
        if crate::auth::all_lists_empty(&[&self.allowed_user_ids, &self.allowed_role_ids]) {
            return true;
        }
        self.allowed_user_ids.contains(&msg.author_id)
            || msg
                .author_roles
                .iter()
                .any(|r| self.allowed_role_ids.contains(r))
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.discord.send_or_edit", skip_all, level = "debug", fields(buf_len = %self.buffer.len()))
    )]
    async fn send_or_edit(&mut self) -> Result<(), ChannelError> {
        let channel_id = self
            .channel_id
            .clone()
            .ok_or(ChannelError::NoActiveSession)?;

        let text = if self.buffer.is_empty() {
            "...".to_owned()
        } else {
            self.buffer.text().to_owned()
        };

        if text.len() > MAX_MESSAGE_LEN {
            let chunks = crate::markdown::utf8_chunks(&text, MAX_MESSAGE_LEN);
            for chunk in chunks {
                self.rest
                    .send_message(&channel_id, chunk)
                    .await
                    .map_err(ChannelError::other)?;
            }
            self.message_id = None;
            return Ok(());
        }

        match self.message_id.clone() {
            None => {
                let msg = self
                    .rest
                    .send_message(&channel_id, &text)
                    .await
                    .map_err(ChannelError::other)?;
                self.message_id = Some(msg.id);
            }
            Some(msg_id) => {
                if let Err(e) = self.rest.edit_message(&channel_id, &msg_id, &text).await {
                    tracing::warn!("discord edit failed: {e}, sending new message");
                    self.message_id = None;
                    let msg = self
                        .rest
                        .send_message(&channel_id, &text)
                        .await
                        .map_err(ChannelError::other)?;
                    self.message_id = Some(msg.id);
                }
            }
        }

        self.buffer.mark_flushed();
        Ok(())
    }
}

impl StreamingSend for DiscordChannel {
    async fn send_or_edit(&mut self) -> Result<(), ChannelError> {
        Self::send_or_edit(self).await
    }

    fn streaming_buffer(&self) -> &StreamingBuffer {
        &self.buffer
    }

    fn streaming_buffer_mut(&mut self) -> &mut StreamingBuffer {
        &mut self.buffer
    }

    fn has_pending_message(&self) -> bool {
        self.message_id.is_some()
    }

    fn clear_pending_message(&mut self) {
        self.message_id = None;
    }
}

impl crate::confirm::ConfirmLoop for DiscordChannel {
    type Incoming = IncomingMessage;

    fn confirm_label(&self) -> &'static str {
        "discord"
    }

    fn confirm_receiver(&mut self) -> &mut mpsc::Receiver<IncomingMessage> {
        &mut self.rx
    }

    fn confirm_accepts(&self, incoming: &IncomingMessage) -> bool {
        self.is_authorized(incoming)
    }

    fn confirm_reply_text<'a>(&self, incoming: &'a IncomingMessage) -> &'a str {
        &incoming.content
    }

    async fn confirm_send_prompt(&mut self, text: &str) -> Result<(), ChannelError> {
        self.send(text).await
    }
}

impl Channel for DiscordChannel {
    fn supports_exit(&self) -> bool {
        false
    }

    /// Returns `true` — Discord users are external, untrusted input. The residual text
    /// that survives command dispatch is sanitized centrally in `Agent::run` before it
    /// reaches the LLM context (see `Channel::requires_input_sanitization`).
    fn requires_input_sanitization(&self) -> bool {
        true
    }

    fn try_recv(&mut self) -> Option<ChannelMessage> {
        loop {
            let incoming = self.rx.try_recv().ok()?;
            if !self.is_authorized(&incoming) {
                tracing::warn!(
                    "rejected discord message from unauthorized user: {}",
                    incoming.author_id
                );
                continue;
            }
            self.channel_id = Some(incoming.channel_id);
            return Some(ChannelMessage {
                text: incoming.content,
                attachments: vec![],
                is_guest_context: false,
                is_from_bot: false,
                owner_key: None,
            });
        }
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.discord.recv", skip_all, fields(msg_len = tracing::field::Empty))
    )]
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        loop {
            let Some(incoming) = self.rx.recv().await else {
                return Ok(None);
            };

            if !self.is_authorized(&incoming) {
                tracing::warn!(
                    "rejected discord message from unauthorized user: {}",
                    incoming.author_id
                );
                continue;
            }

            self.channel_id = Some(incoming.channel_id);
            self.buffer.reset();
            self.message_id = None;

            return Ok(Some(ChannelMessage {
                text: incoming.content,
                attachments: vec![],
                is_guest_context: false,
                is_from_bot: false,
                owner_key: None,
            }));
        }
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.discord.send", skip_all, fields(msg_len = %text.len()))
    )]
    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        let channel_id = self
            .channel_id
            .as_deref()
            .ok_or(ChannelError::NoActiveSession)?;

        if text.len() <= MAX_MESSAGE_LEN {
            self.rest
                .send_message(channel_id, text)
                .await
                .map_err(ChannelError::other)?;
        } else {
            let chunks = crate::markdown::utf8_chunks(text, MAX_MESSAGE_LEN);
            for chunk in chunks {
                self.rest
                    .send_message(channel_id, chunk)
                    .await
                    .map_err(ChannelError::other)?;
            }
        }
        Ok(())
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.discord.send_chunk", skip_all, level = "debug", fields(chunk_len = %chunk.len()))
    )]
    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        StreamingSend::streaming_send_chunk(self, chunk).await
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.discord.flush_chunks", skip_all, level = "debug")
    )]
    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        StreamingSend::streaming_flush_chunks(self).await
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.discord.send_typing", skip_all, level = "debug")
    )]
    async fn send_typing(&mut self) -> Result<(), ChannelError> {
        let Some(channel_id) = self.channel_id.as_deref() else {
            return Ok(());
        };
        let _ = self.rest.trigger_typing(channel_id).await;
        Ok(())
    }

    async fn send_status(&mut self, text: &str) -> Result<(), ChannelError> {
        if text.is_empty() {
            return Ok(());
        }
        let Some(channel_id) = self.channel_id.as_deref() else {
            return Ok(());
        };
        self.rest
            .send_message(channel_id, text)
            .await
            .map_err(ChannelError::other)?;
        Ok(())
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.discord.confirm", skip_all, fields(prompt_len = %prompt.len()))
    )]
    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        crate::confirm::ConfirmLoop::run_confirm(self, prompt).await
    }

    fn elicit(
        &mut self,
        request: ElicitationRequest,
    ) -> impl std::future::Future<Output = Result<ElicitationResponse, ChannelError>> + Send {
        tracing::warn!(
            server = %request.server_name,
            "elicit() not supported on Discord channel — declining"
        );
        std::future::ready(Ok(ElicitationResponse::Declined))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_channel() -> DiscordChannel {
        let (_tx, rx) = mpsc::channel(16);
        let rest = rest::RestClient::new("test-token".into());
        DiscordChannel {
            rx,
            rest,
            _gateway_handle: Some(tokio::spawn(std::future::pending())),
            channel_id: None,
            allowed_user_ids: vec![],
            allowed_role_ids: vec![],
            allowed_channel_ids: vec![],
            buffer: StreamingBuffer::new(EDIT_THROTTLE),
            message_id: None,
            supervisor: None,
        }
    }

    fn make_incoming(author_id: &str, channel_id: &str, roles: Vec<String>) -> IncomingMessage {
        IncomingMessage {
            channel_id: channel_id.into(),
            content: "hello".into(),
            author_id: author_id.into(),
            author_roles: roles,
        }
    }

    #[tokio::test]
    async fn is_authorized_allows_all_when_empty_lists() {
        let ch = make_channel();
        let msg = make_incoming("user1", "ch1", vec![]);
        assert!(ch.is_authorized(&msg));
    }

    #[tokio::test]
    async fn is_authorized_rejects_channel_not_in_allowlist() {
        let mut ch = make_channel();
        ch.allowed_channel_ids = vec!["ch-allowed".into()];
        let msg = make_incoming("user1", "ch-other", vec![]);
        assert!(!ch.is_authorized(&msg));
    }

    #[tokio::test]
    async fn is_authorized_allows_channel_in_allowlist() {
        let mut ch = make_channel();
        ch.allowed_channel_ids = vec!["ch1".into()];
        let msg = make_incoming("user1", "ch1", vec![]);
        assert!(ch.is_authorized(&msg));
    }

    #[tokio::test]
    async fn is_authorized_allows_user_in_allowlist() {
        let mut ch = make_channel();
        ch.allowed_user_ids = vec!["user1".into()];
        let msg = make_incoming("user1", "ch1", vec![]);
        assert!(ch.is_authorized(&msg));
    }

    #[tokio::test]
    async fn is_authorized_rejects_user_not_in_allowlist() {
        let mut ch = make_channel();
        ch.allowed_user_ids = vec!["user-other".into()];
        let msg = make_incoming("user1", "ch1", vec![]);
        assert!(!ch.is_authorized(&msg));
    }

    #[tokio::test]
    async fn is_authorized_allows_role_in_allowlist() {
        let mut ch = make_channel();
        ch.allowed_role_ids = vec!["admin".into()];
        let msg = make_incoming("user1", "ch1", vec!["admin".into()]);
        assert!(ch.is_authorized(&msg));
    }

    #[tokio::test]
    async fn is_authorized_rejects_when_no_matching_role_or_user() {
        let mut ch = make_channel();
        ch.allowed_user_ids = vec!["user-other".into()];
        ch.allowed_role_ids = vec!["admin".into()];
        let msg = make_incoming("user1", "ch1", vec!["member".into()]);
        assert!(!ch.is_authorized(&msg));
    }

    #[tokio::test]
    async fn buffer_should_flush_true_when_no_last_edit() {
        let ch = make_channel();
        assert!(ch.buffer.should_flush());
    }

    #[tokio::test]
    async fn buffer_should_flush_false_within_throttle() {
        let mut ch = make_channel();
        ch.buffer.push("x");
        ch.buffer.mark_flushed();
        assert!(!ch.buffer.should_flush());
    }

    #[test]
    fn buffer_should_flush_true_after_throttle() {
        let mut buf = StreamingBuffer::new(Duration::from_millis(1));
        buf.push("x");
        buf.mark_flushed();
        std::thread::sleep(Duration::from_millis(5));
        assert!(buf.should_flush());
    }

    #[tokio::test]
    async fn send_chunk_accumulates() {
        let mut ch = make_channel();
        ch.buffer.push("hello ");
        ch.buffer.push("world");
        assert_eq!(ch.buffer.text(), "hello world");
    }

    #[tokio::test]
    async fn flush_chunks_clears_state() {
        let mut ch = make_channel();
        ch.buffer.push("test");
        ch.buffer.mark_flushed();
        // message_id is None, buffer not empty — send_or_edit will be called but fails without REST
        // So reset state manually and check post-condition directly:
        // Re-init with empty buffer and no message_id to test the clear-only path.
        let mut ch2 = make_channel();
        // message_id is None, buffer is empty — send_or_edit is NOT called
        ch2.flush_chunks().await.unwrap();
        assert!(ch2.buffer.is_empty());
        assert!(ch2.message_id.is_none());
    }

    #[tokio::test]
    async fn try_recv_sets_channel_id() {
        let (tx, rx) = mpsc::channel(16);
        let rest = rest::RestClient::new("test-token".into());
        let mut ch = DiscordChannel {
            rx,
            rest,
            _gateway_handle: Some(tokio::spawn(std::future::pending())),
            channel_id: None,
            allowed_user_ids: vec![],
            allowed_role_ids: vec![],
            allowed_channel_ids: vec![],
            buffer: StreamingBuffer::new(EDIT_THROTTLE),
            message_id: None,
            supervisor: None,
        };
        tx.try_send(make_incoming("user1", "ch42", vec![])).unwrap();
        let msg = ch.try_recv().unwrap();
        assert_eq!(msg.text, "hello");
        assert_eq!(ch.channel_id.as_deref(), Some("ch42"));
    }

    /// Regression test for #5460: `recv`/`try_recv` must return raw, unsanitized text so
    /// command dispatch in `Agent::run` still matches recognized commands over Discord.
    /// Sanitization for the residual non-command text happens centrally in the agent loop,
    /// gated on `requires_input_sanitization`, not at the channel adapter.
    #[tokio::test]
    async fn requires_input_sanitization_is_true() {
        let ch = make_channel();
        assert!(ch.requires_input_sanitization());
    }

    #[tokio::test]
    async fn try_recv_skips_unauthorized() {
        let (tx, rx) = mpsc::channel(16);
        let rest = rest::RestClient::new("test-token".into());
        let mut ch = DiscordChannel {
            rx,
            rest,
            _gateway_handle: Some(tokio::spawn(std::future::pending())),
            channel_id: None,
            allowed_user_ids: vec!["allowed-user".into()],
            allowed_role_ids: vec![],
            allowed_channel_ids: vec![],
            buffer: StreamingBuffer::new(EDIT_THROTTLE),
            message_id: None,
            supervisor: None,
        };
        tx.try_send(make_incoming("unauthorized", "ch1", vec![]))
            .unwrap();
        assert!(ch.try_recv().is_none());
    }

    #[tokio::test]
    async fn debug_impl() {
        let ch = make_channel();
        let debug = format!("{ch:?}");
        assert!(debug.contains("DiscordChannel"));
    }

    #[test]
    fn max_message_len_constant() {
        assert_eq!(MAX_MESSAGE_LEN, 2000);
    }

    #[test]
    fn edit_throttle_constant() {
        assert_eq!(EDIT_THROTTLE, Duration::from_millis(1500));
    }

    #[tokio::test]
    async fn confirm_returns_err_without_active_channel() {
        // confirm() calls send() first. Without channel_id, send() returns
        // Err("no active channel") and confirm() propagates it via `?`.
        // This test verifies that confirm() is callable and errors correctly.
        let mut ch = make_channel();
        // channel_id is None in make_channel() — send() will fail immediately.
        let result = ch.confirm("delete everything?").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn confirm_timeout_logic_denies_on_timeout() {
        // Verify the timeout + recv logic used inside confirm() in isolation.
        // Full integration testing of confirm() (including the send() REST call)
        // requires a mock HTTP server and is covered by live agent testing.
        tokio::time::pause();
        let (_tx, mut rx) = mpsc::channel::<IncomingMessage>(1);
        // Advance past CONFIRM_TIMEOUT while _tx is still alive (no message sent).
        let timeout_fut = tokio::time::timeout(crate::CONFIRM_TIMEOUT, rx.recv());
        tokio::time::advance(crate::CONFIRM_TIMEOUT + Duration::from_millis(1)).await;
        let result = timeout_fut.await;
        // Should time out (Err), not receive a message.
        assert!(result.is_err(), "expected timeout Err, got recv result");
    }

    #[tokio::test]
    async fn confirm_skips_unauthorized_and_accepts_authorized() {
        tokio::time::pause();
        let (tx, rx) = mpsc::channel(16);
        let rest = rest::RestClient::new("test-token".into());
        let mut ch = DiscordChannel {
            rx,
            rest,
            _gateway_handle: Some(tokio::spawn(std::future::pending())),
            channel_id: Some("ch1".into()),
            allowed_user_ids: vec!["allowed-user".into()],
            allowed_role_ids: vec![],
            allowed_channel_ids: vec![],
            buffer: StreamingBuffer::new(EDIT_THROTTLE),
            message_id: None,
            supervisor: None,
        };
        // Unauthorized message first, then authorized "yes".
        tx.try_send(make_incoming("intruder", "ch1", vec![]))
            .unwrap();
        tx.try_send(make_incoming("allowed-user", "ch1", vec![]))
            .unwrap();
        // Test the loop logic directly (confirm() calls send() which needs REST).
        let deadline = tokio::time::Instant::now() + crate::CONFIRM_TIMEOUT;
        let mut confirmed = false;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "timed out unexpectedly");
            match tokio::time::timeout(remaining, ch.rx.recv()).await {
                Ok(Some(msg)) => {
                    if !ch.is_authorized(&msg) {
                        continue;
                    }
                    confirmed = msg.content.trim().eq_ignore_ascii_case("hello");
                    break;
                }
                Ok(None) | Err(_) => break,
            }
        }
        assert!(confirmed);
    }

    /// Regression test for #6472: an unconfigured allowlist must refuse to
    /// start (fail-closed), matching Telegram's `start()` semantics, instead
    /// of silently accepting messages from any user.
    #[test]
    fn new_rejects_empty_allowlists() {
        let result = DiscordChannel::new("test-token".into(), vec![], vec![], vec![], None);
        assert!(matches!(result, Err(ChannelError::Other(_))));
    }

    /// A non-empty role allowlist alone is sufficient to pass the startup gate,
    /// even when `allowed_user_ids` is empty — mirrors `is_authorized`'s
    /// role-only path.
    #[tokio::test]
    async fn new_allows_role_only_allowlist() {
        let result = DiscordChannel::new(
            "test-token".into(),
            vec![],
            vec!["admin-role".into()],
            vec![],
            None,
        );
        assert!(result.is_ok());
    }
}
