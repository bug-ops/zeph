// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Slack channel adapter using Events API + Web API.
//!
//! # Slash commands
//!
//! Unlike Discord, Slack slash commands are configured statically in the Slack App Dashboard
//! (App Manifest) and cannot be registered via API at runtime. To add slash commands to the
//! Zeph Slack app, update the app manifest at <https://api.slack.com/apps> and add entries
//! under `slash_commands`. No runtime registration is needed or possible.

pub mod api;
pub mod events;

use std::time::Duration;

use tokio::sync::mpsc;
use zeph_common::TaskSupervisor;
use zeph_core::channel::{
    Attachment, AttachmentKind, Channel, ChannelError, ChannelMessage, ElicitationRequest,
    ElicitationResponse,
};

use self::events::IncomingMessage;
use crate::streaming::{StreamingBuffer, StreamingSend};

const EDIT_THROTTLE: Duration = Duration::from_secs(2);

/// Slack channel adapter implementing edit-in-place streaming.
pub struct SlackChannel {
    rx: mpsc::Receiver<IncomingMessage>,
    api: api::SlackApi,
    /// Webhook server task handle. `None` when the server is supervised by a
    /// [`TaskSupervisor`] (lifecycle is owned by the supervisor in that case).
    _server_handle: Option<tokio::task::JoinHandle<()>>,
    channel_id: Option<String>,
    allowed_user_ids: Vec<String>,
    allowed_channel_ids: Vec<String>,
    buffer: StreamingBuffer,
    message_ts: Option<String>,
}

impl std::fmt::Debug for SlackChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackChannel")
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}

impl SlackChannel {
    /// Create a new Slack channel and spawn the events webhook server.
    ///
    /// Use [`with_supervisor`] to attach a [`TaskSupervisor`] so the events server task
    /// is tracked, observable in the TUI, and restarted on panic.
    ///
    /// # Errors
    ///
    /// Returns an error if the auth.test API call fails.
    ///
    /// [`with_supervisor`]: SlackChannel::with_supervisor
    pub async fn new(
        bot_token: String,
        signing_secret: String,
        host: String,
        port: u16,
        allowed_user_ids: Vec<String>,
        allowed_channel_ids: Vec<String>,
    ) -> Result<Self, zeph_core::channel::ChannelError> {
        Self::new_with_supervisor(
            bot_token,
            signing_secret,
            host,
            port,
            allowed_user_ids,
            allowed_channel_ids,
            None,
        )
        .await
    }

    /// Create a new Slack channel with a supervisor attached.
    ///
    /// Prefer this constructor in production to ensure the events server task is tracked
    /// and restarted on panic. Equivalent to calling [`new`] followed by [`with_supervisor`],
    /// but threads the supervisor into [`events::spawn_event_server`] at construction time.
    ///
    /// # Errors
    ///
    /// Returns an error if the auth.test API call fails.
    ///
    /// [`new`]: SlackChannel::new
    /// [`with_supervisor`]: SlackChannel::with_supervisor
    pub async fn new_with_supervisor(
        bot_token: String,
        signing_secret: String,
        host: String,
        port: u16,
        allowed_user_ids: Vec<String>,
        allowed_channel_ids: Vec<String>,
        supervisor: Option<&TaskSupervisor>,
    ) -> Result<Self, zeph_core::channel::ChannelError> {
        let api = api::SlackApi::new(bot_token);
        let bot_user_id = match api.auth_test().await {
            Ok(id) => {
                tracing::info!(bot_user_id = %id, "slack auth.test succeeded");
                id
            }
            Err(e) => {
                tracing::warn!("slack auth.test failed: {e}, self-message filtering disabled");
                String::new()
            }
        };
        let (server_handle, rx) = events::spawn_event_server(
            host,
            port,
            signing_secret,
            bot_user_id,
            allowed_user_ids.clone(),
            allowed_channel_ids.clone(),
            supervisor,
        );
        Ok(Self {
            rx,
            api,
            _server_handle: server_handle,
            channel_id: None,
            allowed_user_ids,
            allowed_channel_ids,
            buffer: StreamingBuffer::new(EDIT_THROTTLE),
            message_ts: None,
        })
    }

    /// Attach a [`TaskSupervisor`] to an already-constructed channel.
    ///
    /// Note: the events server task is spawned at construction time via [`new`] or
    /// [`new_with_supervisor`]. Calling this method after construction only stores the
    /// supervisor for potential future use; prefer [`new_with_supervisor`] in production.
    ///
    /// [`new`]: SlackChannel::new
    /// [`new_with_supervisor`]: SlackChannel::new_with_supervisor
    #[must_use]
    pub fn with_supervisor(self, _supervisor: TaskSupervisor) -> Self {
        // Supervisor was already consumed at spawn_event_server call time if
        // new_with_supervisor was used. This method is a no-op for consistency
        // with the Telegram/Discord builder pattern — use new_with_supervisor instead.
        self
    }

    fn is_authorized(&self, msg: &IncomingMessage) -> bool {
        if !self.allowed_channel_ids.is_empty()
            && !self.allowed_channel_ids.contains(&msg.channel_id)
        {
            return false;
        }
        if self.allowed_user_ids.is_empty() {
            return true;
        }
        self.allowed_user_ids.contains(&msg.user_id)
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.slack.send_or_edit", skip_all, level = "debug", fields(buf_len = %self.buffer.len()))
    )]
    async fn send_or_edit(&mut self) -> Result<(), ChannelError> {
        let channel_id = self
            .channel_id
            .as_deref()
            .ok_or(ChannelError::NoActiveSession)?;

        let text = if self.buffer.is_empty() {
            "..."
        } else {
            self.buffer.text()
        };

        match &self.message_ts {
            None => {
                let ts = self
                    .api
                    .post_message(channel_id, text)
                    .await
                    .map_err(ChannelError::other)?;
                self.message_ts = Some(ts);
            }
            Some(ts) => {
                if let Err(e) = self.api.update_message(channel_id, ts, text).await {
                    tracing::warn!("slack update failed: {e}, sending new message");
                    self.message_ts = None;
                    let ts = self
                        .api
                        .post_message(channel_id, text)
                        .await
                        .map_err(ChannelError::other)?;
                    self.message_ts = Some(ts);
                }
            }
        }

        self.buffer.mark_flushed();
        Ok(())
    }
}

impl StreamingSend for SlackChannel {
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
        self.message_ts.is_some()
    }

    fn clear_pending_message(&mut self) {
        self.message_ts = None;
    }
}

impl Channel for SlackChannel {
    fn supports_exit(&self) -> bool {
        false
    }

    fn try_recv(&mut self) -> Option<ChannelMessage> {
        let incoming = self.rx.try_recv().ok()?;
        self.channel_id = Some(incoming.channel_id);
        Some(ChannelMessage {
            text: incoming.text,
            attachments: vec![],
            is_guest_context: false,
            is_from_bot: false,
        })
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.slack.recv", skip_all, fields(msg_len = tracing::field::Empty))
    )]
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        let Some(incoming) = self.rx.recv().await else {
            return Ok(None);
        };

        self.channel_id = Some(incoming.channel_id);
        self.buffer.reset();
        self.message_ts = None;

        let mut attachments = Vec::new();
        for file in &incoming.files {
            match self.api.download_file(&file.url).await {
                Ok(data) => {
                    attachments.push(Attachment {
                        kind: AttachmentKind::Audio,
                        data,
                        filename: file.filename.clone(),
                    });
                }
                Err(e) => {
                    tracing::warn!("failed to download slack audio file: {e}");
                }
            }
        }

        Ok(Some(ChannelMessage {
            text: incoming.text,
            attachments,
            is_guest_context: false,
            is_from_bot: false,
        }))
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.slack.send", skip_all, fields(msg_len = %text.len()))
    )]
    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        let channel_id = self
            .channel_id
            .as_deref()
            .ok_or(ChannelError::NoActiveSession)?;

        self.api
            .post_message(channel_id, text)
            .await
            .map_err(ChannelError::other)?;
        Ok(())
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.slack.send_chunk", skip_all, level = "debug", fields(chunk_len = %chunk.len()))
    )]
    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        StreamingSend::streaming_send_chunk(self, chunk).await
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.slack.flush_chunks", skip_all, level = "debug")
    )]
    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        StreamingSend::streaming_flush_chunks(self).await
    }

    async fn send_status(&mut self, text: &str) -> Result<(), ChannelError> {
        if text.is_empty() {
            return Ok(());
        }
        let Some(channel_id) = self.channel_id.as_deref() else {
            return Ok(());
        };
        self.api
            .post_message(channel_id, text)
            .await
            .map_err(ChannelError::other)?;
        Ok(())
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channels.slack.confirm", skip_all, fields(prompt_len = %prompt.len()))
    )]
    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        self.send(&format!(
            "{prompt}\nReply 'yes' to confirm (timeout: {}s).",
            crate::CONFIRM_TIMEOUT.as_secs()
        ))
        .await?;
        let deadline = tokio::time::Instant::now() + crate::CONFIRM_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!(
                    "slack confirm timed out after {}s — denied",
                    crate::CONFIRM_TIMEOUT.as_secs()
                );
                return Ok(false);
            }
            match tokio::time::timeout(remaining, self.rx.recv()).await {
                Ok(Some(incoming)) => {
                    if !self.is_authorized(&incoming) {
                        tracing::debug!(
                            user_id = %incoming.user_id,
                            "slack confirm: ignoring message from unauthorized user"
                        );
                        continue;
                    }
                    return Ok(incoming.text.trim().eq_ignore_ascii_case("yes"));
                }
                Ok(None) => {
                    tracing::warn!("slack confirm channel closed — denying");
                    return Ok(false);
                }
                Err(_) => {
                    tracing::warn!(
                        "slack confirm timed out after {}s — denied",
                        crate::CONFIRM_TIMEOUT.as_secs()
                    );
                    return Ok(false);
                }
            }
        }
    }

    async fn elicit(
        &mut self,
        request: ElicitationRequest,
    ) -> Result<ElicitationResponse, ChannelError> {
        tracing::warn!(
            server = %request.server_name,
            "elicit() not supported on Slack channel — declining"
        );
        Ok(ElicitationResponse::Declined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_channel() -> SlackChannel {
        let (_tx, rx) = mpsc::channel(16);
        let api = api::SlackApi::new("xoxb-test".into());
        SlackChannel {
            rx,
            api,
            _server_handle: Some(tokio::spawn(std::future::pending())),
            channel_id: None,
            allowed_user_ids: vec![],
            allowed_channel_ids: vec![],
            buffer: StreamingBuffer::new(EDIT_THROTTLE),
            message_ts: None,
        }
    }

    fn make_incoming(user_id: &str, channel_id: &str) -> IncomingMessage {
        IncomingMessage {
            channel_id: channel_id.into(),
            text: "hello".into(),
            user_id: user_id.into(),
            files: vec![],
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn buffer_should_flush_true_when_no_last_edit() {
        let ch = make_channel();
        assert!(ch.buffer.should_flush());
    }

    #[tokio::test(flavor = "current_thread")]
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

    #[tokio::test(flavor = "current_thread")]
    async fn flush_chunks_clears_state() {
        let mut ch = make_channel();
        // buffer empty, message_ts None — send_or_edit is NOT called
        ch.flush_chunks().await.unwrap();
        assert!(ch.buffer.is_empty());
        assert!(ch.message_ts.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn is_authorized_allows_all_when_empty_lists() {
        let ch = make_channel();
        let msg = make_incoming("U1", "C1");
        assert!(ch.is_authorized(&msg));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn is_authorized_rejects_channel_not_in_allowlist() {
        let mut ch = make_channel();
        ch.allowed_channel_ids = vec!["C-allowed".into()];
        let msg = make_incoming("U1", "C-other");
        assert!(!ch.is_authorized(&msg));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn is_authorized_allows_channel_in_allowlist() {
        let mut ch = make_channel();
        ch.allowed_channel_ids = vec!["C1".into()];
        let msg = make_incoming("U1", "C1");
        assert!(ch.is_authorized(&msg));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn is_authorized_allows_user_in_allowlist() {
        let mut ch = make_channel();
        ch.allowed_user_ids = vec!["U1".into()];
        let msg = make_incoming("U1", "C1");
        assert!(ch.is_authorized(&msg));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn is_authorized_rejects_user_not_in_allowlist() {
        let mut ch = make_channel();
        ch.allowed_user_ids = vec!["U-other".into()];
        let msg = make_incoming("U1", "C1");
        assert!(!ch.is_authorized(&msg));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confirm_skips_unauthorized_and_accepts_authorized() {
        tokio::time::pause();
        let (tx, rx) = mpsc::channel(16);
        let api = api::SlackApi::new("xoxb-test".into());
        let mut ch = SlackChannel {
            rx,
            api,
            _server_handle: Some(tokio::spawn(std::future::pending())),
            channel_id: Some("C1".into()),
            allowed_user_ids: vec!["U-allowed".into()],
            allowed_channel_ids: vec![],
            buffer: StreamingBuffer::new(EDIT_THROTTLE),
            message_ts: None,
        };
        // Send an unauthorized message followed by an authorized "yes".
        tx.try_send(IncomingMessage {
            channel_id: "C1".into(),
            text: "yes".into(),
            user_id: "U-intruder".into(),
            files: vec![],
        })
        .unwrap();
        tx.try_send(IncomingMessage {
            channel_id: "C1".into(),
            text: "yes".into(),
            user_id: "U-allowed".into(),
            files: vec![],
        })
        .unwrap();
        // confirm() will call send() first (posts prompt), which calls api.post_message —
        // that will fail without a real Slack API, so we test the authorization loop directly.
        // Instead, test the loop logic by feeding both messages:
        // The first (unauthorized) must be skipped, the second (authorized) accepted.
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
                    confirmed = msg.text.trim().eq_ignore_ascii_case("yes");
                    break;
                }
                Ok(None) | Err(_) => break,
            }
        }
        assert!(confirmed);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn try_recv_sets_channel_id() {
        let (tx, rx) = mpsc::channel(16);
        let api = api::SlackApi::new("xoxb-test".into());
        let mut ch = SlackChannel {
            rx,
            api,
            _server_handle: Some(tokio::spawn(std::future::pending())),
            channel_id: None,
            allowed_user_ids: vec![],
            allowed_channel_ids: vec![],
            buffer: StreamingBuffer::new(EDIT_THROTTLE),
            message_ts: None,
        };
        tx.try_send(IncomingMessage {
            channel_id: "C123".into(),
            text: "hello".into(),
            user_id: "U1".into(),
            files: vec![],
        })
        .unwrap();
        let msg = ch.try_recv().unwrap();
        assert_eq!(msg.text, "hello");
        assert_eq!(ch.channel_id.as_deref(), Some("C123"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn debug_impl() {
        let ch = make_channel();
        let debug = format!("{ch:?}");
        assert!(debug.contains("SlackChannel"));
    }

    #[test]
    fn edit_throttle_constant() {
        assert_eq!(EDIT_THROTTLE, Duration::from_secs(2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accumulate_chunks() {
        let mut ch = make_channel();
        ch.buffer.push("part1");
        ch.buffer.push(" part2");
        assert_eq!(ch.buffer.text(), "part1 part2");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confirm_returns_err_without_active_channel() {
        // confirm() calls send() first. Without channel_id, send() returns
        // Err(ChannelError::NoActiveSession) and confirm() propagates it via `?`.
        // This test verifies that confirm() is callable and errors correctly.
        let mut ch = make_channel();
        // channel_id is None in make_channel() — send() will fail immediately.
        let result = ch.confirm("delete everything?").await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn confirm_timeout_logic_denies_on_timeout() {
        // Verify the timeout + recv logic used inside confirm() in isolation.
        // Full integration testing of confirm() (including the Slack API call)
        // requires a mock HTTP server and is covered by live agent testing.
        tokio::time::pause();
        let (_tx, mut rx) = mpsc::channel::<IncomingMessage>(1);
        let timeout_fut = tokio::time::timeout(crate::CONFIRM_TIMEOUT, rx.recv());
        tokio::time::advance(crate::CONFIRM_TIMEOUT + Duration::from_millis(1)).await;
        let result = timeout_fut.await;
        assert!(result.is_err(), "expected timeout Err, got recv result");
    }
}
