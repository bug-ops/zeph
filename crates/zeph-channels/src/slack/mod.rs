//! Slack channel adapter using Events API + Web API.

pub mod api;
pub mod events;

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use zeph_core::channel::{Attachment, AttachmentKind, Channel, ChannelError, ChannelMessage};

use self::events::IncomingMessage;

const EDIT_THROTTLE: Duration = Duration::from_secs(2);

/// Slack channel adapter implementing edit-in-place streaming.
pub struct SlackChannel {
    rx: mpsc::Receiver<IncomingMessage>,
    api: api::SlackApi,
    channel_id: Option<String>,
    accumulated: String,
    last_edit: Option<Instant>,
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
    /// # Errors
    ///
    /// Returns an error if the auth.test API call fails.
    pub async fn new(
        bot_token: String,
        signing_secret: String,
        host: String,
        port: u16,
        allowed_user_ids: Vec<String>,
        allowed_channel_ids: Vec<String>,
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
        let rx = events::spawn_event_server(
            host,
            port,
            signing_secret,
            bot_user_id,
            allowed_user_ids,
            allowed_channel_ids,
        );
        Ok(Self {
            rx,
            api,
            channel_id: None,
            accumulated: String::new(),
            last_edit: None,
            message_ts: None,
        })
    }

    fn should_send_update(&self) -> bool {
        self.last_edit
            .is_none_or(|last| last.elapsed() > EDIT_THROTTLE)
    }

    async fn send_or_edit(&mut self) -> Result<(), ChannelError> {
        let channel_id = self
            .channel_id
            .as_deref()
            .ok_or_else(|| ChannelError::Other("no active channel".into()))?;

        let text = if self.accumulated.is_empty() {
            "..."
        } else {
            &self.accumulated
        };

        match &self.message_ts {
            None => {
                let ts = self
                    .api
                    .post_message(channel_id, text)
                    .await
                    .map_err(|e| ChannelError::Other(e.to_string()))?;
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
                        .map_err(|e| ChannelError::Other(e.to_string()))?;
                    self.message_ts = Some(ts);
                }
            }
        }

        self.last_edit = Some(Instant::now());
        Ok(())
    }
}

impl Channel for SlackChannel {
    fn try_recv(&mut self) -> Option<ChannelMessage> {
        let incoming = self.rx.try_recv().ok()?;
        self.channel_id = Some(incoming.channel_id);
        Some(ChannelMessage {
            text: incoming.text,
            attachments: vec![],
        })
    }

    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        let Some(incoming) = self.rx.recv().await else {
            return Ok(None);
        };

        self.channel_id = Some(incoming.channel_id);
        self.accumulated.clear();
        self.last_edit = None;
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
        }))
    }

    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        let channel_id = self
            .channel_id
            .as_deref()
            .ok_or_else(|| ChannelError::Other("no active channel".into()))?;

        self.api
            .post_message(channel_id, text)
            .await
            .map_err(|e| ChannelError::Other(e.to_string()))?;
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
        if self.message_ts.is_some() {
            self.send_or_edit().await?;
        }
        self.accumulated.clear();
        self.last_edit = None;
        self.message_ts = None;
        Ok(())
    }

    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        self.send(&format!("{prompt}\nReply 'yes' to confirm."))
            .await?;
        let Some(incoming) = self.rx.recv().await else {
            return Ok(false);
        };
        Ok(incoming.text.trim().eq_ignore_ascii_case("yes"))
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
            channel_id: None,
            accumulated: String::new(),
            last_edit: None,
            message_ts: None,
        }
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
        ch.last_edit = Some(Instant::now() - Duration::from_secs(3));
        assert!(ch.should_send_update());
    }

    #[tokio::test]
    async fn flush_chunks_clears_state() {
        let mut ch = make_channel();
        ch.accumulated = "test".into();
        ch.last_edit = Some(Instant::now());
        // message_ts is None, so send_or_edit won't be called
        ch.flush_chunks().await.unwrap();
        assert!(ch.accumulated.is_empty());
        assert!(ch.last_edit.is_none());
        assert!(ch.message_ts.is_none());
    }

    #[test]
    fn try_recv_sets_channel_id() {
        let (tx, rx) = mpsc::channel(16);
        let api = api::SlackApi::new("xoxb-test".into());
        let mut ch = SlackChannel {
            rx,
            api,
            channel_id: None,
            accumulated: String::new(),
            last_edit: None,
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

    #[test]
    fn debug_impl() {
        let ch = make_channel();
        let debug = format!("{ch:?}");
        assert!(debug.contains("SlackChannel"));
    }

    #[test]
    fn edit_throttle_constant() {
        assert_eq!(EDIT_THROTTLE, Duration::from_secs(2));
    }

    #[test]
    fn accumulate_chunks() {
        let mut ch = make_channel();
        ch.accumulated.push_str("part1");
        ch.accumulated.push_str(" part2");
        assert_eq!(ch.accumulated, "part1 part2");
    }
}
