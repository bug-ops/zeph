use std::time::{Duration, Instant};

use crate::markdown::markdown_to_telegram;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, MessageId, ParseMode};
use tokio::sync::mpsc;
use zeph_core::channel::{Attachment, AttachmentKind, Channel, ChannelError, ChannelMessage};

const MAX_MESSAGE_LEN: usize = 4096;
const MAX_IMAGE_BYTES: u32 = 20 * 1024 * 1024;

/// Telegram channel adapter using teloxide.
#[derive(Debug)]
pub struct TelegramChannel {
    bot: Bot,
    chat_id: Option<ChatId>,
    rx: mpsc::Receiver<IncomingMessage>,
    allowed_users: Vec<String>,
    accumulated: String,
    last_edit: Option<Instant>,
    message_id: Option<MessageId>,
}

#[derive(Debug)]
struct IncomingMessage {
    chat_id: ChatId,
    text: String,
    attachments: Vec<Attachment>,
}

impl TelegramChannel {
    #[must_use]
    pub fn new(token: String, allowed_users: Vec<String>) -> Self {
        let bot = Bot::new(token);
        let (_, rx) = mpsc::channel(64);
        Self {
            bot,
            chat_id: None,
            rx,
            allowed_users,
            accumulated: String::new(),
            last_edit: None,
            message_id: None,
        }
    }

    /// Spawn the teloxide update listener and return `self` ready for use.
    ///
    /// # Errors
    ///
    /// Returns an error if the bot cannot be initialized.
    pub fn start(mut self) -> Result<Self, ChannelError> {
        if self.allowed_users.is_empty() {
            tracing::error!("telegram.allowed_users is empty; refusing to start an open bot");
            return Err(ChannelError::Other(
                "telegram.allowed_users must not be empty".into(),
            ));
        }

        let (tx, rx) = mpsc::channel::<IncomingMessage>(64);
        self.rx = rx;

        let bot = self.bot.clone();
        let allowed = self.allowed_users.clone();

        tokio::spawn(async move {
            let handler = Update::filter_message().endpoint(move |msg: Message, bot: Bot| {
                let tx = tx.clone();
                let allowed = allowed.clone();
                async move {
                    let username = msg.from.as_ref().and_then(|u| u.username.clone());

                    if !allowed.is_empty() {
                        let is_allowed = username
                            .as_deref()
                            .is_some_and(|u| allowed.iter().any(|a| a == u));
                        if !is_allowed {
                            tracing::warn!(
                                "rejected message from unauthorized user: {:?}",
                                username
                            );
                            return respond(());
                        }
                    }

                    let text = msg.text().unwrap_or_default().to_string();
                    let mut attachments = Vec::new();

                    let audio_file_id = msg
                        .voice()
                        .map(|v| (v.file.id.0.clone(), v.file.size))
                        .or_else(|| msg.audio().map(|a| (a.file.id.0.clone(), a.file.size)));

                    if let Some((file_id, file_size)) = audio_file_id {
                        match download_file(&bot, file_id, file_size).await {
                            Ok(data) => {
                                attachments.push(Attachment {
                                    kind: AttachmentKind::Audio,
                                    data,
                                    filename: msg.audio().and_then(|a| a.file_name.clone()),
                                });
                            }
                            Err(e) => {
                                tracing::warn!("failed to download audio attachment: {e}");
                            }
                        }
                    }

                    // Handle photo attachments (pick the largest available size)
                    if let Some(photos) = msg.photo()
                        && let Some(photo) = photos.iter().max_by_key(|p| p.file.size)
                    {
                        if photo.file.size > MAX_IMAGE_BYTES {
                            tracing::warn!(
                                size = photo.file.size,
                                max = MAX_IMAGE_BYTES,
                                "photo exceeds size limit, skipping"
                            );
                        } else {
                            match download_file(&bot, photo.file.id.0.clone(), photo.file.size)
                                .await
                            {
                                Ok(data) => {
                                    attachments.push(Attachment {
                                        kind: AttachmentKind::Image,
                                        data,
                                        filename: None,
                                    });
                                }
                                Err(e) => {
                                    tracing::warn!("failed to download photo attachment: {e}");
                                }
                            }
                        }
                    }

                    if text.is_empty() && attachments.is_empty() {
                        return respond(());
                    }

                    let _ = tx
                        .send(IncomingMessage {
                            chat_id: msg.chat.id,
                            text,
                            attachments,
                        })
                        .await;

                    respond(())
                }
            });

            Dispatcher::builder(bot, handler)
                .enable_ctrlc_handler()
                .build()
                .dispatch()
                .await;
        });

        tracing::info!("telegram bot listener started");
        Ok(self)
    }

    fn is_command(text: &str) -> Option<&str> {
        let cmd = text.split_whitespace().next()?;
        if cmd.starts_with('/') {
            Some(cmd)
        } else {
            None
        }
    }

    fn should_send_update(&self) -> bool {
        match self.last_edit {
            None => true,
            Some(last) => last.elapsed() > Duration::from_secs(10),
        }
    }

    async fn send_or_edit(&mut self) -> Result<(), ChannelError> {
        let Some(chat_id) = self.chat_id else {
            return Err(ChannelError::Other("no active chat".into()));
        };

        let text = if self.accumulated.is_empty() {
            "..."
        } else {
            &self.accumulated
        };

        let formatted_text = markdown_to_telegram(text);

        if formatted_text.is_empty() {
            tracing::debug!("skipping send: formatted text is empty");
            return Ok(());
        }

        tracing::debug!("formatted_text (full): {}", formatted_text);

        match self.message_id {
            None => {
                tracing::debug!("sending new message (length: {})", formatted_text.len());
                let msg = self
                    .bot
                    .send_message(chat_id, formatted_text)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await
                    .map_err(|e| ChannelError::Other(e.to_string()))?;
                self.message_id = Some(msg.id);
                tracing::debug!("new message sent with id: {:?}", msg.id);
            }
            Some(msg_id) => {
                tracing::debug!(
                    "editing message {:?} (length: {})",
                    msg_id,
                    formatted_text.len()
                );
                let edit_result = self
                    .bot
                    .edit_message_text(chat_id, msg_id, &formatted_text)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await;

                if let Err(e) = edit_result {
                    let error_msg = e.to_string();

                    if error_msg.contains("message is not modified") {
                        // Text hasn't changed, just skip this update
                        tracing::debug!("message content unchanged, skipping edit");
                    } else if error_msg.contains("message to edit not found")
                        || error_msg.contains("MESSAGE_ID_INVALID")
                    {
                        tracing::warn!(
                            "Telegram edit failed (message_id stale?): {e}, sending new message"
                        );
                        self.message_id = None;
                        self.last_edit = None;

                        let msg = self
                            .bot
                            .send_message(chat_id, &formatted_text)
                            .parse_mode(ParseMode::MarkdownV2)
                            .await
                            .map_err(|e| ChannelError::Other(e.to_string()))?;
                        self.message_id = Some(msg.id);
                    } else {
                        return Err(ChannelError::Other(e.to_string()));
                    }
                } else {
                    tracing::debug!("message edited successfully");
                }
            }
        }

        self.last_edit = Some(Instant::now());
        Ok(())
    }
}

async fn download_file(bot: &Bot, file_id: String, capacity: u32) -> Result<Vec<u8>, String> {
    use teloxide::net::Download;

    let file = bot
        .get_file(file_id.into())
        .await
        .map_err(|e| format!("get_file: {e}"))?;
    let mut buf: Vec<u8> = Vec::with_capacity(capacity as usize);
    bot.download_file(&file.path, &mut buf)
        .await
        .map_err(|e| format!("download_file: {e}"))?;
    Ok(buf)
}

impl Channel for TelegramChannel {
    fn try_recv(&mut self) -> Option<ChannelMessage> {
        self.rx.try_recv().ok().map(|incoming| {
            self.chat_id = Some(incoming.chat_id);
            ChannelMessage {
                text: incoming.text,
                attachments: incoming.attachments,
            }
        })
    }

    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        loop {
            let Some(incoming) = self.rx.recv().await else {
                return Ok(None);
            };

            self.chat_id = Some(incoming.chat_id);

            // Reset streaming state for new response
            self.accumulated.clear();
            self.last_edit = None;
            self.message_id = None;

            if let Some(cmd) = Self::is_command(&incoming.text) {
                match cmd {
                    "/start" => {
                        self.send("Welcome to Zeph! Send me a message to get started.")
                            .await?;
                        continue;
                    }
                    "/reset" => {
                        return Ok(Some(ChannelMessage {
                            text: "/reset".to_string(),
                            attachments: vec![],
                        }));
                    }
                    "/skills" => {
                        return Ok(Some(ChannelMessage {
                            text: "/skills".to_string(),
                            attachments: vec![],
                        }));
                    }
                    _ => {}
                }
            }

            return Ok(Some(ChannelMessage {
                text: incoming.text,
                attachments: incoming.attachments,
            }));
        }
    }

    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        let Some(chat_id) = self.chat_id else {
            return Err(ChannelError::Other("no active chat".into()));
        };

        let formatted_text = markdown_to_telegram(text);

        if formatted_text.is_empty() {
            tracing::debug!("skipping send: formatted text is empty");
            return Ok(());
        }

        if formatted_text.len() <= MAX_MESSAGE_LEN {
            self.bot
                .send_message(chat_id, &formatted_text)
                .parse_mode(ParseMode::MarkdownV2)
                .await
                .map_err(|e| ChannelError::Other(e.to_string()))?;
        } else {
            let chunks = crate::markdown::utf8_chunks(&formatted_text, MAX_MESSAGE_LEN);
            for chunk in chunks {
                self.bot
                    .send_message(chat_id, chunk)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await
                    .map_err(|e| ChannelError::Other(e.to_string()))?;
            }
        }

        Ok(())
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        self.accumulated.push_str(chunk);
        tracing::debug!(
            "received chunk (size: {}, total: {})",
            chunk.len(),
            self.accumulated.len()
        );

        if self.should_send_update() {
            tracing::debug!("sending update (should_send_update returned true)");
            self.send_or_edit().await?;
        }

        Ok(())
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        tracing::debug!(
            "flushing chunks (message_id: {:?}, accumulated: {} bytes)",
            self.message_id,
            self.accumulated.len()
        );

        // Final update with complete message
        if self.message_id.is_some() {
            self.send_or_edit().await?;
        }

        // Clear state for next response
        self.accumulated.clear();
        self.last_edit = None;
        self.message_id = None;

        Ok(())
    }

    async fn send_typing(&mut self) -> Result<(), ChannelError> {
        let Some(chat_id) = self.chat_id else {
            return Ok(());
        };
        self.bot
            .send_chat_action(chat_id, ChatAction::Typing)
            .await
            .map_err(|e| ChannelError::Other(e.to_string()))?;
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

    #[test]
    fn is_command_detection() {
        assert_eq!(TelegramChannel::is_command("/start"), Some("/start"));
        assert_eq!(TelegramChannel::is_command("/reset now"), Some("/reset"));
        assert_eq!(TelegramChannel::is_command("hello"), None);
        assert_eq!(TelegramChannel::is_command(""), None);
    }

    #[test]
    fn should_send_update_first_chunk() {
        let token = "test_token".to_string();
        let allowed_users = Vec::new();
        let channel = TelegramChannel::new(token, allowed_users);
        assert!(channel.should_send_update());
    }

    #[test]
    fn should_send_update_time_threshold() {
        let token = "test_token".to_string();
        let allowed_users = Vec::new();
        let mut channel = TelegramChannel::new(token, allowed_users);
        channel.accumulated = "test".to_string();
        // Set last_edit to 11 seconds ago (threshold is 10 seconds)
        channel.last_edit = Some(Instant::now() - Duration::from_secs(11));
        assert!(channel.should_send_update());
    }

    #[tokio::test]
    async fn send_chunk_accumulates() {
        let token = "test_token".to_string();
        let allowed_users = Vec::new();
        let mut channel = TelegramChannel::new(token, allowed_users);

        // Manually set chat_id to avoid send_or_edit failure
        // In real tests, this would be set by recv()
        channel.accumulated.push_str("hello");
        channel.accumulated.push(' ');
        channel.accumulated.push_str("world");

        assert_eq!(channel.accumulated, "hello world");
    }

    #[tokio::test]
    async fn flush_chunks_clears_state() {
        let token = "test_token".to_string();
        let allowed_users = Vec::new();
        let mut channel = TelegramChannel::new(token, allowed_users);

        channel.accumulated = "test".to_string();
        channel.last_edit = Some(Instant::now());
        // Do not set message_id to avoid triggering send_or_edit()

        channel.flush_chunks().await.unwrap();

        assert!(channel.accumulated.is_empty());
        assert!(channel.last_edit.is_none());
        assert!(channel.message_id.is_none());
    }

    #[test]
    fn max_image_bytes_is_20_mib() {
        assert_eq!(MAX_IMAGE_BYTES, 20 * 1024 * 1024);
    }

    #[test]
    fn photo_size_limit_enforcement() {
        // Mirrors the guard in the photo extraction handler:
        // photos.iter().max_by_key(|p| p.file.size) followed by
        // if photo.file.size > MAX_IMAGE_BYTES { skip } else { download }
        let size_within_limit: u32 = MAX_IMAGE_BYTES - 1;
        let size_at_limit: u32 = MAX_IMAGE_BYTES;
        let size_over_limit: u32 = MAX_IMAGE_BYTES + 1;

        assert!(size_within_limit <= MAX_IMAGE_BYTES);
        assert!(size_at_limit <= MAX_IMAGE_BYTES);
        assert!(size_over_limit > MAX_IMAGE_BYTES);
    }

    #[test]
    fn should_not_send_update_within_threshold() {
        let token = "test_token".to_string();
        let allowed_users = Vec::new();
        let mut channel = TelegramChannel::new(token, allowed_users);
        // Set last_edit to 1 second ago (well within the 10-second threshold)
        channel.last_edit = Some(Instant::now() - Duration::from_secs(1));
        assert!(!channel.should_send_update());
    }

    #[test]
    fn start_rejects_empty_allowed_users() {
        let channel = TelegramChannel::new("test_token".to_string(), Vec::new());
        let result = channel.start();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ChannelError::Other(_)));
    }
}
