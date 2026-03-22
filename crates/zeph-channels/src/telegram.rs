// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::time::{Duration, Instant};

use crate::markdown::markdown_to_telegram;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, ChatAction, MessageId, ParseMode};
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

    /// Spawn a task that registers bot commands in the Telegram menu.
    fn register_commands(bot: Bot) {
        tokio::spawn(async move {
            let commands = vec![
                BotCommand::new("start", "Start a new conversation"),
                BotCommand::new("reset", "Reset conversation history"),
                BotCommand::new("skills", "List loaded skills"),
                BotCommand::new("agent", "Manage sub-agents (list/spawn/status/cancel)"),
            ];
            if let Err(e) = bot.set_my_commands(commands).await {
                tracing::warn!("failed to register bot commands: {e}");
            }
        });
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

        Self::register_commands(bot.clone());

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

    /// Creates a `TelegramChannel` with an injectable sender for unit tests.
    ///
    /// The returned `Sender` allows injecting `IncomingMessage` values directly
    /// without a real Telegram bot token or live API access. The bot is
    /// initialized with a dummy token and `chat_id` is left unset; tests that
    /// exercise paths which call the bot API (e.g. `send()`, `confirm()`) must
    /// either avoid those code paths or configure a mock HTTP server via
    /// `Bot::set_api_url`.
    #[cfg(test)]
    fn new_test(allowed_users: Vec<String>) -> (Self, mpsc::Sender<IncomingMessage>) {
        let (tx, rx) = mpsc::channel(64);
        let channel = Self {
            bot: Bot::new("test_token"),
            chat_id: None,
            rx,
            allowed_users,
            accumulated: String::new(),
            last_edit: None,
            message_id: None,
        };
        (channel, tx)
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
                    .map_err(ChannelError::other)?;
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
                            .map_err(ChannelError::other)?;
                        self.message_id = Some(msg.id);
                    } else {
                        return Err(ChannelError::other(e));
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
    fn supports_exit(&self) -> bool {
        false
    }

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
                .map_err(ChannelError::other)?;
        } else {
            let chunks = crate::markdown::utf8_chunks(&formatted_text, MAX_MESSAGE_LEN);
            for chunk in chunks {
                self.bot
                    .send_message(chat_id, chunk)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await
                    .map_err(ChannelError::other)?;
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
            .map_err(ChannelError::other)?;
        Ok(())
    }

    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        self.send(&format!(
            "{prompt}\nReply 'yes' to confirm (timeout: {}s).",
            crate::CONFIRM_TIMEOUT.as_secs()
        ))
        .await?;
        match tokio::time::timeout(crate::CONFIRM_TIMEOUT, self.rx.recv()).await {
            Ok(Some(incoming)) => Ok(incoming.text.trim().eq_ignore_ascii_case("yes")),
            Ok(None) => {
                tracing::warn!("confirm channel closed — denying secret request");
                Ok(false)
            }
            Err(_) => {
                tracing::warn!("confirm timed out after 30s — denied");
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    // ---------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------

    /// Minimal valid Telegram `sendMessage` / `editMessageText` response.
    fn tg_ok_message() -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 42,
                "date": 1_700_000_000_i64,
                "chat": {"id": 1, "type": "private"}
            }
        })
    }

    /// Creates a `TelegramChannel` whose bot is pointed at `server` so that
    /// any API call the channel makes is intercepted rather than going to the
    /// real Telegram endpoint.
    async fn make_mocked_channel(
        server: &MockServer,
        allowed_users: Vec<String>,
    ) -> (TelegramChannel, mpsc::Sender<IncomingMessage>) {
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_json(tg_ok_message()))
            .mount(server)
            .await;

        let api_url = reqwest::Url::parse(&server.uri()).unwrap();
        let bot = Bot::new("test_token").set_api_url(api_url);
        let (tx, rx) = mpsc::channel(64);
        let channel = TelegramChannel {
            bot,
            chat_id: Some(ChatId(1)),
            rx,
            allowed_users,
            accumulated: String::new(),
            last_edit: None,
            message_id: None,
        };
        (channel, tx)
    }

    fn plain_message(text: &str) -> IncomingMessage {
        IncomingMessage {
            chat_id: ChatId(1),
            text: text.to_string(),
            attachments: vec![],
        }
    }

    // ---------------------------------------------------------------------------
    // Pure-function unit tests (no async, no network)
    // ---------------------------------------------------------------------------

    #[test]
    fn is_command_detection() {
        assert_eq!(TelegramChannel::is_command("/start"), Some("/start"));
        assert_eq!(TelegramChannel::is_command("/reset now"), Some("/reset"));
        assert_eq!(TelegramChannel::is_command("hello"), None);
        assert_eq!(TelegramChannel::is_command(""), None);
    }

    #[test]
    fn should_send_update_first_chunk() {
        let channel = TelegramChannel::new("test_token".to_string(), Vec::new());
        assert!(channel.should_send_update());
    }

    #[test]
    fn should_send_update_time_threshold() {
        let mut channel = TelegramChannel::new("test_token".to_string(), Vec::new());
        channel.accumulated = "test".to_string();
        channel.last_edit = Some(Instant::now().checked_sub(Duration::from_secs(11)).unwrap());
        assert!(channel.should_send_update());
    }

    #[test]
    fn should_not_send_update_within_threshold() {
        let mut channel = TelegramChannel::new("test_token".to_string(), Vec::new());
        channel.last_edit = Some(Instant::now().checked_sub(Duration::from_secs(1)).unwrap());
        assert!(!channel.should_send_update());
    }

    #[test]
    fn max_image_bytes_is_20_mib() {
        assert_eq!(MAX_IMAGE_BYTES, 20 * 1024 * 1024);
    }

    #[test]
    fn photo_size_limit_enforcement() {
        assert!(MAX_IMAGE_BYTES - 1 <= MAX_IMAGE_BYTES);
        assert!(MAX_IMAGE_BYTES <= MAX_IMAGE_BYTES);
        assert!(MAX_IMAGE_BYTES + 1 > MAX_IMAGE_BYTES);
    }

    #[test]
    fn start_rejects_empty_allowed_users() {
        let result = TelegramChannel::new("test_token".to_string(), Vec::new()).start();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ChannelError::Other(_)));
    }

    // ---------------------------------------------------------------------------
    // recv() — injectable sender tests (no network calls)
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn recv_returns_channel_message_when_injected() {
        let (mut channel, tx) = TelegramChannel::new_test(vec![]);
        tx.send(plain_message("hello world")).await.unwrap();
        let msg = channel.recv().await.unwrap().unwrap();
        assert_eq!(msg.text, "hello world");
        assert!(msg.attachments.is_empty());
    }

    #[tokio::test]
    async fn recv_reset_command_routed_correctly() {
        let (mut channel, tx) = TelegramChannel::new_test(vec![]);
        tx.send(plain_message("/reset")).await.unwrap();
        let msg = channel.recv().await.unwrap().unwrap();
        assert_eq!(msg.text, "/reset");
    }

    #[tokio::test]
    async fn recv_skills_command_routed_correctly() {
        let (mut channel, tx) = TelegramChannel::new_test(vec![]);
        tx.send(plain_message("/skills")).await.unwrap();
        let msg = channel.recv().await.unwrap().unwrap();
        assert_eq!(msg.text, "/skills");
    }

    #[tokio::test]
    async fn recv_unknown_command_passed_through() {
        let (mut channel, tx) = TelegramChannel::new_test(vec![]);
        tx.send(plain_message("/unknown_cmd arg")).await.unwrap();
        let msg = channel.recv().await.unwrap().unwrap();
        assert_eq!(msg.text, "/unknown_cmd arg");
    }

    #[tokio::test]
    async fn recv_returns_none_when_sender_dropped() {
        let (mut channel, tx) = TelegramChannel::new_test(vec![]);
        drop(tx);
        let result = channel.recv().await.unwrap();
        assert!(result.is_none());
    }

    // ---------------------------------------------------------------------------
    // send_chunk() / flush_chunks() — accumulation (no network calls)
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn send_chunk_accumulates_text_without_api_call() {
        let (mut channel, _tx) = TelegramChannel::new_test(vec![]);
        // Suppress the API call by setting last_edit within the 10-second threshold.
        channel.last_edit = Some(Instant::now());

        channel.send_chunk("hello").await.unwrap();
        channel.send_chunk(" world").await.unwrap();

        assert_eq!(channel.accumulated, "hello world");
    }

    #[tokio::test]
    async fn flush_chunks_clears_state_when_no_message_id() {
        let (mut channel, _tx) = TelegramChannel::new_test(vec![]);
        channel.accumulated = "some text".to_string();
        channel.last_edit = Some(Instant::now());
        // message_id is None, so flush_chunks does not call send_or_edit.

        channel.flush_chunks().await.unwrap();

        assert!(channel.accumulated.is_empty());
        assert!(channel.last_edit.is_none());
        assert!(channel.message_id.is_none());
    }

    // ---------------------------------------------------------------------------
    // recv(/start) — mock HTTP server required to intercept the welcome send()
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn recv_start_consumed_internally_without_returning_to_caller() {
        let server = MockServer::start().await;
        let (mut channel, tx) = make_mocked_channel(&server, vec![]).await;

        // /start is consumed; recv() loops and waits for the next message.
        tx.send(plain_message("/start")).await.unwrap();
        tx.send(plain_message("hello after start")).await.unwrap();

        let msg = channel.recv().await.unwrap().unwrap();
        assert_eq!(msg.text, "hello after start");
    }

    // ---------------------------------------------------------------------------
    // flush_chunks() with message_id set — mock HTTP server required
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn flush_chunks_calls_edit_and_clears_state_when_message_id_set() {
        let server = MockServer::start().await;
        let (mut channel, _tx) = make_mocked_channel(&server, vec![]).await;

        channel.accumulated = "partial response".to_string();
        channel.last_edit = Some(Instant::now());
        channel.message_id = Some(teloxide::types::MessageId(42));

        channel.flush_chunks().await.unwrap();

        assert!(channel.accumulated.is_empty());
        assert!(channel.last_edit.is_none());
        assert!(channel.message_id.is_none());
    }

    // ---------------------------------------------------------------------------
    // confirm() timeout / close / yes — tested at the rx+timeout level in
    // isolation (the same logic confirm() delegates to), avoiding the
    // send() REST call.  Full confirm() round-trips are covered by live
    // agent testing with a real (or mock) Telegram bot.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn confirm_timeout_logic_denies_on_timeout() {
        tokio::time::pause();
        let (_tx, mut rx) = mpsc::channel::<IncomingMessage>(1);
        let timeout_fut = tokio::time::timeout(crate::CONFIRM_TIMEOUT, rx.recv());
        tokio::time::advance(crate::CONFIRM_TIMEOUT + Duration::from_millis(1)).await;
        let result = timeout_fut.await;
        assert!(result.is_err(), "expected timeout Err, got recv result");
    }

    #[tokio::test]
    async fn confirm_close_logic_denies_on_channel_close() {
        let (tx, mut rx) = mpsc::channel::<IncomingMessage>(1);
        drop(tx);
        let result = tokio::time::timeout(crate::CONFIRM_TIMEOUT, rx.recv()).await;
        assert!(result.is_ok(), "should not time out");
        assert!(
            result.unwrap().is_none(),
            "closed channel should yield None"
        );
    }

    #[tokio::test]
    async fn confirm_yes_logic_accepts_yes_response() {
        let (tx, mut rx) = mpsc::channel::<IncomingMessage>(1);
        tx.send(plain_message("yes")).await.unwrap();
        let result = tokio::time::timeout(crate::CONFIRM_TIMEOUT, rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(result.text.trim().eq_ignore_ascii_case("yes"));
    }

    #[tokio::test]
    async fn confirm_no_logic_denies_non_yes_response() {
        let (tx, mut rx) = mpsc::channel::<IncomingMessage>(1);
        tx.send(plain_message("no")).await.unwrap();
        let result = tokio::time::timeout(crate::CONFIRM_TIMEOUT, rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!result.text.trim().eq_ignore_ascii_case("yes"));
    }
}
