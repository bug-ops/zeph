use teloxide::prelude::*;
use teloxide::types::ChatAction;
use tokio::sync::mpsc;
use zeph_core::channel::{Channel, ChannelMessage};

const MAX_MESSAGE_LEN: usize = 4096;

/// Telegram channel adapter using teloxide.
#[derive(Debug)]
pub struct TelegramChannel {
    bot: Bot,
    chat_id: Option<ChatId>,
    rx: mpsc::Receiver<IncomingMessage>,
    allowed_users: Vec<String>,
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
}

impl Channel for TelegramChannel {
    async fn recv(&mut self) -> anyhow::Result<Option<ChannelMessage>> {
        loop {
            let Some(incoming) = self.rx.recv().await else {
                return Ok(None);
            };

            self.chat_id = Some(incoming.chat_id);

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
            anyhow::bail!("no active chat to send message to");
        };

        if text.len() <= MAX_MESSAGE_LEN {
            self.bot.send_message(chat_id, text).await?;
        } else {
            for chunk in text.as_bytes().chunks(MAX_MESSAGE_LEN) {
                let chunk_str = String::from_utf8_lossy(chunk);
                self.bot.send_message(chat_id, chunk_str.as_ref()).await?;
            }
        }

        Ok(())
    }

    async fn send_chunk(&mut self, _chunk: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn flush_chunks(&mut self) -> anyhow::Result<()> {
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
}
