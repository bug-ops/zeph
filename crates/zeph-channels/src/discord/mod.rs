//! Discord channel adapter using Gateway WebSocket + REST API.

pub mod gateway;
pub mod rest;

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use zeph_core::channel::{Channel, ChannelError, ChannelMessage};

use self::gateway::IncomingMessage;

const MAX_MESSAGE_LEN: usize = 2000;
const EDIT_THROTTLE: Duration = Duration::from_millis(1500);

/// Discord channel adapter implementing edit-in-place streaming.
pub struct DiscordChannel {
    rx: mpsc::Receiver<IncomingMessage>,
    rest: rest::RestClient,
    channel_id: Option<String>,
    allowed_user_ids: Vec<String>,
    allowed_role_ids: Vec<String>,
    allowed_channel_ids: Vec<String>,
    accumulated: String,
    last_edit: Option<Instant>,
    message_id: Option<String>,
}

impl std::fmt::Debug for DiscordChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordChannel")
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}

impl DiscordChannel {
    /// Create a new Discord channel and spawn the gateway listener.
    #[must_use]
    pub fn new(
        token: String,
        allowed_user_ids: Vec<String>,
        allowed_role_ids: Vec<String>,
        allowed_channel_ids: Vec<String>,
    ) -> Self {
        let rx = gateway::spawn_gateway(token.clone());
        let rest = rest::RestClient::new(token);
        Self {
            rx,
            rest,
            channel_id: None,
            allowed_user_ids,
            allowed_role_ids,
            allowed_channel_ids,
            accumulated: String::new(),
            last_edit: None,
            message_id: None,
        }
    }

    fn is_authorized(&self, msg: &IncomingMessage) -> bool {
        if !self.allowed_channel_ids.is_empty()
            && !self.allowed_channel_ids.contains(&msg.channel_id)
        {
            return false;
        }
        if self.allowed_user_ids.is_empty() && self.allowed_role_ids.is_empty() {
            return true;
        }
        if self.allowed_user_ids.contains(&msg.author_id) {
            return true;
        }
        msg.author_roles
            .iter()
            .any(|r| self.allowed_role_ids.contains(r))
    }

    fn should_send_update(&self) -> bool {
        self.last_edit
            .is_none_or(|last| last.elapsed() > EDIT_THROTTLE)
    }

    async fn send_or_edit(&mut self) -> Result<(), ChannelError> {
        let channel_id = self
            .channel_id
            .clone()
            .ok_or_else(|| ChannelError::Other("no active channel".into()))?;

        let text = if self.accumulated.is_empty() {
            "...".to_owned()
        } else {
            self.accumulated.clone()
        };

        if text.len() > MAX_MESSAGE_LEN {
            let chunks = crate::markdown::utf8_chunks(&text, MAX_MESSAGE_LEN);
            for chunk in chunks {
                self.rest
                    .send_message(&channel_id, chunk)
                    .await
                    .map_err(|e| ChannelError::Other(e.to_string()))?;
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
                    .map_err(|e| ChannelError::Other(e.to_string()))?;
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
                        .map_err(|e| ChannelError::Other(e.to_string()))?;
                    self.message_id = Some(msg.id);
                }
            }
        }

        self.last_edit = Some(Instant::now());
        Ok(())
    }
}

impl Channel for DiscordChannel {
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
            });
        }
    }

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
            self.accumulated.clear();
            self.last_edit = None;
            self.message_id = None;

            return Ok(Some(ChannelMessage {
                text: incoming.content,
                attachments: vec![],
            }));
        }
    }

    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        let channel_id = self
            .channel_id
            .as_deref()
            .ok_or_else(|| ChannelError::Other("no active channel".into()))?;

        if text.len() <= MAX_MESSAGE_LEN {
            self.rest
                .send_message(channel_id, text)
                .await
                .map_err(|e| ChannelError::Other(e.to_string()))?;
        } else {
            let chunks = crate::markdown::utf8_chunks(text, MAX_MESSAGE_LEN);
            for chunk in chunks {
                self.rest
                    .send_message(channel_id, chunk)
                    .await
                    .map_err(|e| ChannelError::Other(e.to_string()))?;
            }
        }
        Ok(())
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        self.accumulated.push_str(chunk);
        if self.should_send_update() {
            self.send_or_edit().await?;
        }
        Ok(())
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        if self.message_id.is_some() {
            self.send_or_edit().await?;
        }
        self.accumulated.clear();
        self.last_edit = None;
        self.message_id = None;
        Ok(())
    }

    async fn send_typing(&mut self) -> Result<(), ChannelError> {
        let Some(channel_id) = self.channel_id.as_deref() else {
            return Ok(());
        };
        let _ = self.rest.trigger_typing(channel_id).await;
        Ok(())
    }

    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        self.send(&format!("{prompt}\nReply 'yes' to confirm."))
            .await?;
        let Some(incoming) = self.rx.recv().await else {
            return Ok(false);
        };
        Ok(incoming.content.trim().eq_ignore_ascii_case("yes"))
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
            channel_id: None,
            allowed_user_ids: vec![],
            allowed_role_ids: vec![],
            allowed_channel_ids: vec![],
            accumulated: String::new(),
            last_edit: None,
            message_id: None,
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

    #[test]
    fn is_authorized_allows_all_when_empty_lists() {
        let ch = make_channel();
        let msg = make_incoming("user1", "ch1", vec![]);
        assert!(ch.is_authorized(&msg));
    }

    #[test]
    fn is_authorized_rejects_channel_not_in_allowlist() {
        let mut ch = make_channel();
        ch.allowed_channel_ids = vec!["ch-allowed".into()];
        let msg = make_incoming("user1", "ch-other", vec![]);
        assert!(!ch.is_authorized(&msg));
    }

    #[test]
    fn is_authorized_allows_channel_in_allowlist() {
        let mut ch = make_channel();
        ch.allowed_channel_ids = vec!["ch1".into()];
        let msg = make_incoming("user1", "ch1", vec![]);
        assert!(ch.is_authorized(&msg));
    }

    #[test]
    fn is_authorized_allows_user_in_allowlist() {
        let mut ch = make_channel();
        ch.allowed_user_ids = vec!["user1".into()];
        let msg = make_incoming("user1", "ch1", vec![]);
        assert!(ch.is_authorized(&msg));
    }

    #[test]
    fn is_authorized_rejects_user_not_in_allowlist() {
        let mut ch = make_channel();
        ch.allowed_user_ids = vec!["user-other".into()];
        let msg = make_incoming("user1", "ch1", vec![]);
        assert!(!ch.is_authorized(&msg));
    }

    #[test]
    fn is_authorized_allows_role_in_allowlist() {
        let mut ch = make_channel();
        ch.allowed_role_ids = vec!["admin".into()];
        let msg = make_incoming("user1", "ch1", vec!["admin".into()]);
        assert!(ch.is_authorized(&msg));
    }

    #[test]
    fn is_authorized_rejects_when_no_matching_role_or_user() {
        let mut ch = make_channel();
        ch.allowed_user_ids = vec!["user-other".into()];
        ch.allowed_role_ids = vec!["admin".into()];
        let msg = make_incoming("user1", "ch1", vec!["member".into()]);
        assert!(!ch.is_authorized(&msg));
    }

    #[test]
    fn should_send_update_true_when_no_last_edit() {
        let ch = make_channel();
        assert!(ch.should_send_update());
    }

    #[test]
    fn should_send_update_false_within_throttle() {
        let mut ch = make_channel();
        ch.last_edit = Some(Instant::now());
        assert!(!ch.should_send_update());
    }

    #[test]
    fn should_send_update_true_after_throttle() {
        let mut ch = make_channel();
        ch.last_edit = Some(Instant::now() - Duration::from_millis(1600));
        assert!(ch.should_send_update());
    }

    #[test]
    fn send_chunk_accumulates() {
        let mut ch = make_channel();
        ch.accumulated.push_str("hello ");
        ch.accumulated.push_str("world");
        assert_eq!(ch.accumulated, "hello world");
    }

    #[tokio::test]
    async fn flush_chunks_clears_state() {
        let mut ch = make_channel();
        ch.accumulated = "test".into();
        ch.last_edit = Some(Instant::now());
        // message_id is None, so send_or_edit won't be called
        ch.flush_chunks().await.unwrap();
        assert!(ch.accumulated.is_empty());
        assert!(ch.last_edit.is_none());
        assert!(ch.message_id.is_none());
    }

    #[test]
    fn try_recv_sets_channel_id() {
        let (tx, rx) = mpsc::channel(16);
        let rest = rest::RestClient::new("test-token".into());
        let mut ch = DiscordChannel {
            rx,
            rest,
            channel_id: None,
            allowed_user_ids: vec![],
            allowed_role_ids: vec![],
            allowed_channel_ids: vec![],
            accumulated: String::new(),
            last_edit: None,
            message_id: None,
        };
        tx.try_send(make_incoming("user1", "ch42", vec![])).unwrap();
        let msg = ch.try_recv().unwrap();
        assert_eq!(msg.text, "hello");
        assert_eq!(ch.channel_id.as_deref(), Some("ch42"));
    }

    #[test]
    fn try_recv_skips_unauthorized() {
        let (tx, rx) = mpsc::channel(16);
        let rest = rest::RestClient::new("test-token".into());
        let mut ch = DiscordChannel {
            rx,
            rest,
            channel_id: None,
            allowed_user_ids: vec!["allowed-user".into()],
            allowed_role_ids: vec![],
            allowed_channel_ids: vec![],
            accumulated: String::new(),
            last_edit: None,
            message_id: None,
        };
        tx.try_send(make_incoming("unauthorized", "ch1", vec![]))
            .unwrap();
        assert!(ch.try_recv().is_none());
    }

    #[test]
    fn debug_impl() {
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
}
