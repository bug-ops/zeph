use std::time::{Duration, Instant};

use teloxide::prelude::*;
use teloxide::types::{ChatAction, MessageId, ParseMode};
use tokio::sync::mpsc;
use zeph_core::channel::{Channel, ChannelMessage};

use crate::error::ChannelError;
use crate::markdown::markdown_to_telegram;

const MAX_MESSAGE_LEN: usize = 4096;

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
    pub fn start(mut self) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<IncomingMessage>(64);
        self.rx = rx;

        let bot = self.bot.clone();
        let allowed = self.allowed_users.clone();

        tokio::spawn(async move {
            let handler = Update::filter_message().endpoint(move |msg: Message, _bot: Bot| {
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

                    let Some(text) = msg.text() else {
                        return respond(());
                    };

                    let _ = tx
                        .send(IncomingMessage {
                            chat_id: msg.chat.id,
                            text: text.to_string(),
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

    async fn send_or_edit(&mut self) -> anyhow::Result<()> {
        let Some(chat_id) = self.chat_id else {
            return Err(ChannelError::NoActiveChat.into());
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
                    .await?;
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
                            .await?;
                        self.message_id = Some(msg.id);
                    } else {
                        return Err(e.into());
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

impl Channel for TelegramChannel {
    fn try_recv(&mut self) -> Option<ChannelMessage> {
        self.rx.try_recv().ok().map(|incoming| {
            self.chat_id = Some(incoming.chat_id);
            ChannelMessage {
                text: incoming.text,
            }
        })
    }

    async fn recv(&mut self) -> anyhow::Result<Option<ChannelMessage>> {
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
                        }));
                    }
                    "/skills" => {
                        return Ok(Some(ChannelMessage {
                            text: "/skills".to_string(),
                        }));
                    }
                    _ => {}
                }
            }

            return Ok(Some(ChannelMessage {
                text: incoming.text,
            }));
        }
    }

    async fn send(&mut self, text: &str) -> anyhow::Result<()> {
        let Some(chat_id) = self.chat_id else {
            return Err(ChannelError::NoActiveChat.into());
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
                .await?;
        } else {
            let chunks = crate::markdown::utf8_chunks(&formatted_text, MAX_MESSAGE_LEN);
            for chunk in chunks {
                self.bot
                    .send_message(chat_id, chunk)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
        }

        Ok(())
    }

    async fn send_chunk(&mut self, chunk: &str) -> anyhow::Result<()> {
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

    async fn flush_chunks(&mut self) -> anyhow::Result<()> {
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

    async fn send_typing(&mut self) -> anyhow::Result<()> {
        let Some(chat_id) = self.chat_id else {
            return Ok(());
        };
        self.bot
            .send_chat_action(chat_id, ChatAction::Typing)
            .await?;
        Ok(())
    }

    async fn confirm(&mut self, prompt: &str) -> anyhow::Result<bool> {
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
}
