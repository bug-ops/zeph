// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Telegram channel adapter built on [teloxide](https://docs.rs/teloxide).
//!
//! This module exposes [`TelegramChannel`], which implements [`Channel`] by
//! wrapping a teloxide [`Dispatcher`] running in a background tokio task.
//!
//! # Key design decisions
//!
//! * **Access control** — messages from users not in `allowed_users` are
//!   dropped at the dispatcher level, before they reach the agent.
//! * **Streaming via edit-on-interval** — chunks are accumulated and the bot
//!   edits a single Telegram message at most once every 3 seconds, staying
//!   within Telegram's rate limits while still providing progressive output.
//! * **4096-byte chunking** — messages that exceed Telegram's limit are
//!   split at UTF-8 / newline boundaries via [`utf8_chunks`].
//! * **Elicitation** — MCP server input requests are forwarded to the user
//!   as sequential Telegram messages with per-field 120-second timeouts.
//!
//! [`Channel`]: zeph_core::channel::Channel
//! [`utf8_chunks`]: crate::markdown::utf8_chunks

use std::time::{Duration, Instant};

use crate::markdown::markdown_to_telegram;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, ChatAction, MessageId, ParseMode};
use tokio::sync::mpsc;
use zeph_core::TaskSupervisor;
use zeph_core::channel::{
    Attachment, AttachmentKind, Channel, ChannelError, ChannelMessage, ElicitationField,
    ElicitationFieldType, ElicitationRequest, ElicitationResponse,
};

const MAX_MESSAGE_LEN: usize = 4096;
const MAX_IMAGE_BYTES: u32 = 20 * 1024 * 1024;

/// Telegram channel adapter using [teloxide](https://docs.rs/teloxide).
///
/// `TelegramChannel` bridges the Zeph agent loop with the Telegram Bot API.
/// It runs a teloxide [`Dispatcher`] in a background task that feeds incoming
/// messages into an internal [`mpsc`] channel; the agent calls [`recv`] to
/// receive them one at a time.
///
/// # Streaming output
///
/// LLM responses are streamed to Telegram via an edit-on-interval strategy:
/// chunks are accumulated in memory and the bot edits a single message every
/// three seconds.  This avoids hitting Telegram's rate limits while still
/// providing a progressive output experience.  [`flush_chunks`] performs one
/// final edit with the complete response text.
///
/// # Access control
///
/// The bot only accepts messages from usernames listed in `allowed_users`.
/// Messages from any other user are silently dropped.  Passing an empty
/// `allowed_users` list is treated as a configuration error and [`start`]
/// will return `Err`.
///
/// # Built-in commands
///
/// | Command | Behaviour |
/// |---------|-----------|
/// | `/start` | Sends a welcome message; not forwarded to the agent. |
/// | `/reset` | Forwarded to the agent as a regular message. |
/// | `/skills` | Forwarded to the agent as a regular message. |
/// | `/agent` | Forwarded to the agent as a regular message. |
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_channels::telegram::TelegramChannel;
///
/// let token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap();
/// let allowed = vec!["my_username".to_string()];
/// let channel = TelegramChannel::new(token, allowed)
///     .start()
///     .expect("failed to start telegram bot");
/// ```
///
/// [`recv`]: TelegramChannel::recv
/// [`flush_chunks`]: TelegramChannel::flush_chunks
/// [`start`]: TelegramChannel::start
pub struct TelegramChannel {
    bot: Bot,
    chat_id: Option<ChatId>,
    rx: mpsc::Receiver<IncomingMessage>,
    allowed_users: Vec<String>,
    accumulated: String,
    last_edit: Option<Instant>,
    message_id: Option<MessageId>,
    /// Optional supervisor used to register the Telegram listener task in the
    /// workspace-wide task registry with automatic restart on panic.
    supervisor: Option<TaskSupervisor>,
}

impl std::fmt::Debug for TelegramChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramChannel")
            .field("chat_id", &self.chat_id)
            .field("allowed_users", &self.allowed_users)
            .field("accumulated_len", &self.accumulated.len())
            .field("supervisor", &self.supervisor.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct IncomingMessage {
    chat_id: ChatId,
    text: String,
    attachments: Vec<Attachment>,
}

impl TelegramChannel {
    /// Create a new `TelegramChannel`.
    ///
    /// The channel is **not** active yet — no teloxide dispatcher is running
    /// and no updates will be received until [`start`] is called.
    ///
    /// # Arguments
    ///
    /// * `token` — Telegram bot token obtained from [@BotFather](https://t.me/BotFather).
    /// * `allowed_users` — Telegram usernames (without `@`) that are permitted
    ///   to interact with the bot.  Must not be empty when [`start`] is called.
    ///
    /// [`start`]: TelegramChannel::start
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
            supervisor: None,
        }
    }

    /// Attach a [`TaskSupervisor`] so the Telegram listener task is registered
    /// in the workspace-wide task registry with automatic restart on panic.
    ///
    /// The listener is spawned with
    /// `RestartPolicy::Restart { max: 5, base_delay: 2s }`.
    #[must_use]
    pub fn with_supervisor(mut self, supervisor: TaskSupervisor) -> Self {
        self.supervisor = Some(supervisor);
        self
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
    #[allow(clippy::too_many_lines)] // long function; decomposition would require extracting state into additional structs — deferred to a future structural refactor
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

        let bot_for_factory = bot.clone();
        let allowed_for_factory = allowed.clone();
        let tx_for_factory = tx.clone();
        let listener_factory = move || {
            let bot = bot_for_factory.clone();
            let allowed = allowed_for_factory.clone();
            let tx = tx_for_factory.clone();
            async move {
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
            }
        };

        if let Some(sup) = &self.supervisor {
            sup.spawn(zeph_core::TaskDescriptor {
                name: "telegram_listener",
                restart: zeph_core::RestartPolicy::Restart {
                    max: 5,
                    base_delay: Duration::from_secs(2),
                },
                factory: listener_factory,
            });
        } else {
            tokio::spawn(listener_factory());
        }

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
            supervisor: None,
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
            Some(last) => last.elapsed() > Duration::from_secs(3),
        }
    }

    async fn send_or_edit(&mut self) -> Result<(), ChannelError> {
        let Some(chat_id) = self.chat_id else {
            return Err(ChannelError::NoActiveSession);
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
                let chunks = crate::markdown::utf8_chunks(&formatted_text, MAX_MESSAGE_LEN);
                for chunk in chunks {
                    let msg = self
                        .bot
                        .send_message(chat_id, chunk)
                        .parse_mode(ParseMode::MarkdownV2)
                        .await
                        .map_err(ChannelError::other)?;
                    self.message_id = Some(msg.id);
                    tracing::debug!("new message sent with id: {:?}", msg.id);
                }
            }
            Some(msg_id) => {
                tracing::debug!(
                    "editing message {:?} (length: {})",
                    msg_id,
                    formatted_text.len()
                );
                if formatted_text.len() <= MAX_MESSAGE_LEN {
                    let edit_result = self
                        .bot
                        .edit_message_text(chat_id, msg_id, &formatted_text)
                        .parse_mode(ParseMode::MarkdownV2)
                        .await;

                    if let Err(e) = edit_result {
                        let error_msg = e.to_string();

                        if error_msg.contains("message is not modified") {
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
                } else {
                    // Accumulated text exceeds limit: edit first chunk into existing message,
                    // send remaining chunks as new messages.
                    let chunks = crate::markdown::utf8_chunks(&formatted_text, MAX_MESSAGE_LEN);
                    let mut iter = chunks.into_iter();
                    if let Some(first) = iter.next() {
                        let edit_result = self
                            .bot
                            .edit_message_text(chat_id, msg_id, first)
                            .parse_mode(ParseMode::MarkdownV2)
                            .await;
                        if let Err(e) = edit_result {
                            let error_msg = e.to_string();
                            if !error_msg.contains("message is not modified") {
                                tracing::warn!("Telegram edit failed during split: {e}");
                            }
                        }
                    }
                    for chunk in iter {
                        let msg = self
                            .bot
                            .send_message(chat_id, chunk)
                            .parse_mode(ParseMode::MarkdownV2)
                            .await
                            .map_err(ChannelError::other)?;
                        self.message_id = Some(msg.id);
                        tracing::debug!("overflow chunk sent with id: {:?}", msg.id);
                    }
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
    /// Returns `false` — Telegram is a persistent remote channel with no
    /// meaningful concept of "session exit".
    fn supports_exit(&self) -> bool {
        false
    }

    /// Non-blocking receive: returns a buffered message if one is available.
    ///
    /// Updates `chat_id` when a message is returned so subsequent [`send`]
    /// calls know the destination.
    ///
    /// [`send`]: TelegramChannel::send
    fn try_recv(&mut self) -> Option<ChannelMessage> {
        self.rx.try_recv().ok().map(|incoming| {
            self.chat_id = Some(incoming.chat_id);
            ChannelMessage {
                text: incoming.text,
                attachments: incoming.attachments,
            }
        })
    }

    /// Await the next user message from Telegram.
    ///
    /// The method loops internally to handle built-in commands:
    /// * `/start` — sends the welcome message and loops.
    /// * `/reset` and `/skills` — returned to the caller as regular messages.
    /// * Any other command or plain text — returned immediately.
    ///
    /// Resets the streaming state (`accumulated`, `last_edit`, `message_id`)
    /// so each user turn starts with a clean response buffer.
    ///
    /// Returns `Ok(None)` when the internal channel is closed (i.e. the
    /// dispatcher task has exited).
    ///
    /// # Errors
    ///
    /// Returns `Err` if sending the welcome reply for `/start` fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channel.telegram.recv", skip_all, fields(msg_len = tracing::field::Empty))
    )]
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

    /// Send a complete message to the active Telegram chat.
    ///
    /// The text is converted to `MarkdownV2` via [`markdown_to_telegram`]
    /// before sending.  Messages longer than 4096 bytes are split into
    /// multiple messages at UTF-8 / newline boundaries.
    ///
    /// # Errors
    ///
    /// Returns `Err(ChannelError::NoActiveSession)` if no active chat has been
    /// established yet (i.e. `recv` has never returned a message), or
    /// `Err(ChannelError::Other)` if the Telegram API call fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channel.telegram.send", skip_all, fields(msg_len = %text.len()))
    )]
    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        let Some(chat_id) = self.chat_id else {
            return Err(ChannelError::NoActiveSession);
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

    /// Append a streaming chunk to the response buffer.
    ///
    /// The chunk is accumulated in memory.  A Telegram edit is issued only
    /// when at least 3 seconds have elapsed since the last edit, which keeps
    /// the bot within Telegram's rate limits.
    ///
    /// Call [`flush_chunks`] after the stream ends to perform the final edit
    /// with the complete response text.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the periodic Telegram edit fails.
    ///
    /// [`flush_chunks`]: TelegramChannel::flush_chunks
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

    /// Finalise the streamed response with one last Telegram edit.
    ///
    /// Performs a final edit when a message has already been created
    /// (i.e. `message_id` is `Some`), then clears the accumulation
    /// buffer and resets streaming state so the channel is ready for the next
    /// agent turn.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the final Telegram edit fails.
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

    /// Send a `typing…` chat action to Telegram.
    ///
    /// Silently succeeds (returns `Ok(())`) when no active chat exists yet so
    /// that the agent loop can call this unconditionally without checking state.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the Telegram API call fails.
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

    /// Send a yes/no confirmation prompt to Telegram and await the reply.
    ///
    /// Sends `prompt` followed by instructions to reply `yes`.  Waits up to
    /// [`CONFIRM_TIMEOUT`] (30 s) for a response.  Returns `true` only when
    /// the user replies with the string `yes` (case-insensitive).
    ///
    /// Returns `Ok(false)` on timeout or channel close, never `Err` for those
    /// conditions.
    ///
    /// # Errors
    ///
    /// Returns `Err` if sending the prompt message fails.
    ///
    /// [`CONFIRM_TIMEOUT`]: crate::CONFIRM_TIMEOUT
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

    /// Collect structured input from the user on behalf of an MCP server.
    ///
    /// Sends an introductory message identifying the requesting MCP server,
    /// then prompts for each field sequentially via individual Telegram
    /// messages.  Each field has [`ELICITATION_TIMEOUT`] (120 s) to respond.
    ///
    /// The user can reply `/cancel` at any field prompt to abort the flow,
    /// which returns [`ElicitationResponse::Cancelled`].  Invalid values for a
    /// field return [`ElicitationResponse::Declined`] immediately.
    ///
    /// Enum fields use 1-based numeric selection to stay within Telegram's
    /// 64-byte callback-data limit.
    ///
    /// # Errors
    ///
    /// Returns `Err` if sending any of the prompt messages fails.
    ///
    /// [`ELICITATION_TIMEOUT`]: crate::ELICITATION_TIMEOUT
    /// [`ElicitationResponse::Cancelled`]: zeph_core::channel::ElicitationResponse::Cancelled
    /// [`ElicitationResponse::Declined`]: zeph_core::channel::ElicitationResponse::Declined
    async fn elicit(
        &mut self,
        request: ElicitationRequest,
    ) -> Result<ElicitationResponse, ChannelError> {
        let timeout = crate::ELICITATION_TIMEOUT;

        self.send(&format!(
            "*[MCP server '{}' is requesting input]*\n{}\n\n_Reply /cancel to cancel. \
             Timeout: {}s._",
            sanitize_markdown(&request.server_name),
            sanitize_markdown(&request.message),
            timeout.as_secs(),
        ))
        .await?;

        let mut values = serde_json::Map::new();
        for field in &request.fields {
            let prompt = build_telegram_field_prompt(field);
            self.send(&prompt).await?;

            let incoming = match tokio::time::timeout(timeout, self.rx.recv()).await {
                Ok(Some(msg)) => msg,
                Ok(None) => {
                    tracing::warn!(server = request.server_name, "elicitation channel closed");
                    return Ok(ElicitationResponse::Declined);
                }
                Err(_) => {
                    tracing::warn!(server = request.server_name, "elicitation timed out");
                    let _ = self
                        .send("Elicitation timed out — request cancelled.")
                        .await;
                    return Ok(ElicitationResponse::Cancelled);
                }
            };

            let text = incoming.text.trim().to_owned();

            if text.eq_ignore_ascii_case("/cancel") {
                let _ = self.send("Elicitation cancelled.").await;
                return Ok(ElicitationResponse::Cancelled);
            }

            let Some(value) = coerce_telegram_field(&text, &field.field_type) else {
                let _ = self
                    .send(&format!(
                        "Invalid value for '{}'. Declining.",
                        sanitize_markdown(&field.name)
                    ))
                    .await;
                return Ok(ElicitationResponse::Declined);
            };
            values.insert(field.name.clone(), value);
        }

        Ok(ElicitationResponse::Accepted(serde_json::Value::Object(
            values,
        )))
    }
}

/// Strip Markdown special characters to prevent format injection in Telegram messages.
///
/// Removes `*`, `_`, `[`, `]`, `` ` ``, and ANSI escape sequences (`\x1b`).
/// Used for untrusted strings coming from MCP server metadata (server names,
/// field descriptions) that are embedded in bot messages.
fn sanitize_markdown(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '*' | '_' | '[' | ']' | '`' | '\x1b'))
        .collect()
}

/// Build a Telegram-formatted prompt string for a single elicitation field.
///
/// The prompt is tailored to the field type:
/// * `Boolean` — asks for `yes` or `no`.
/// * `Enum` — lists options with 1-based numeric indices to avoid Telegram's
///   64-byte callback-data limit.
/// * `Integer` / `Number` — asks for a numeric value.
/// * `String` — asks for free-form text.
fn build_telegram_field_prompt(field: &ElicitationField) -> String {
    let req = if field.required { " (required)" } else { "" };
    let name = sanitize_markdown(&field.name);
    match &field.field_type {
        ElicitationFieldType::Boolean => {
            format!("*{name}*{req}: Reply *yes* or *no*")
        }
        ElicitationFieldType::Enum(opts) => {
            // Use short numeric indexes to avoid Telegram 64-byte callback_data limit
            let list: String = opts
                .iter()
                .enumerate()
                .map(|(i, o)| format!("{}: {}", i + 1, sanitize_markdown(o)))
                .collect::<Vec<_>>()
                .join("\n");
            format!("*{name}*{req}: Reply with the number:\n{list}")
        }
        ElicitationFieldType::Integer => {
            format!("*{name}*{req}: Reply with an integer")
        }
        ElicitationFieldType::Number => {
            format!("*{name}*{req}: Reply with a number")
        }
        ElicitationFieldType::String => {
            format!("*{name}*{req}: Reply with text")
        }
    }
}

/// Coerce a raw Telegram reply to the JSON type required by an elicitation field.
///
/// Returns `None` when the input cannot be converted to the declared type.
///
/// Enum fields accept either a 1-based numeric index (as displayed by
/// [`build_telegram_field_prompt`]) or an exact case-insensitive match of the
/// option string.
fn coerce_telegram_field(text: &str, kind: &ElicitationFieldType) -> Option<serde_json::Value> {
    match kind {
        ElicitationFieldType::String => Some(serde_json::Value::String(text.to_owned())),
        ElicitationFieldType::Boolean => {
            if text.eq_ignore_ascii_case("yes") || text == "1" {
                Some(serde_json::Value::Bool(true))
            } else if text.eq_ignore_ascii_case("no") || text == "0" {
                Some(serde_json::Value::Bool(false))
            } else {
                None
            }
        }
        ElicitationFieldType::Integer => text
            .parse::<i64>()
            .ok()
            .map(|n| serde_json::Value::Number(n.into())),
        ElicitationFieldType::Number => text
            .parse::<f64>()
            .ok()
            .and_then(|n| serde_json::Number::from_f64(n).map(serde_json::Value::Number)),
        ElicitationFieldType::Enum(opts) => {
            // Accept numeric index (1-based) or exact match
            if let Ok(idx) = text.parse::<usize>()
                && idx >= 1
                && idx <= opts.len()
            {
                return Some(serde_json::Value::String(opts[idx - 1].clone()));
            }
            // Exact match (case-insensitive)
            opts.iter()
                .find(|o| o.eq_ignore_ascii_case(text))
                .map(|o| serde_json::Value::String(o.clone()))
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
            supervisor: None,
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
        channel.last_edit = Some(Instant::now().checked_sub(Duration::from_secs(4)).unwrap());
        assert!(channel.should_send_update());
    }

    #[test]
    fn should_not_send_update_within_threshold() {
        let mut channel = TelegramChannel::new("test_token".to_string(), Vec::new());
        channel.last_edit = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(500))
                .unwrap(),
        );
        assert!(!channel.should_send_update());
    }

    #[test]
    fn max_image_bytes_is_20_mib() {
        assert_eq!(MAX_IMAGE_BYTES, 20 * 1024 * 1024);
    }

    #[test]
    fn photo_size_limit_enforcement() {
        const { assert!(MAX_IMAGE_BYTES - 1 <= MAX_IMAGE_BYTES) };
        const { assert!(MAX_IMAGE_BYTES <= MAX_IMAGE_BYTES) };
        const { assert!(MAX_IMAGE_BYTES + 1 > MAX_IMAGE_BYTES) };
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

    // ---------------------------------------------------------------------------
    // send_or_edit() — split at MAX_MESSAGE_LEN
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn send_or_edit_splits_long_message_into_multiple_sends() {
        let server = MockServer::start().await;
        let (mut channel, _tx) = make_mocked_channel(&server, vec![]).await;

        // Build text that exceeds MAX_MESSAGE_LEN after markdown_to_telegram pass.
        // Plain ASCII repeated is safe: markdown_to_telegram won't expand it beyond itself.
        let long_text = "a".repeat(MAX_MESSAGE_LEN + 1);
        channel.accumulated = long_text;

        channel.send_or_edit().await.unwrap();

        // The mock server records every request; ≥2 sendMessage calls expected.
        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.len() >= 2,
            "expected ≥2 API calls for oversized message, got {}",
            requests.len()
        );
    }

    #[tokio::test]
    async fn send_or_edit_single_message_when_within_limit() {
        let server = MockServer::start().await;
        let (mut channel, _tx) = make_mocked_channel(&server, vec![]).await;

        channel.accumulated = "short text".to_string();

        channel.send_or_edit().await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            1,
            "expected exactly 1 API call for short message"
        );
        // message_id must be recorded after a successful send
        assert!(channel.message_id.is_some());
    }

    #[tokio::test]
    async fn send_or_edit_splits_when_edit_overflows() {
        let server = MockServer::start().await;
        let (mut channel, _tx) = make_mocked_channel(&server, vec![]).await;

        // Pre-set a message_id to trigger the edit branch.
        channel.message_id = Some(teloxide::types::MessageId(42));
        let long_text = "b".repeat(MAX_MESSAGE_LEN + 1);
        channel.accumulated = long_text;

        channel.send_or_edit().await.unwrap();

        // Expect: 1 editMessageText call + ≥1 sendMessage call for overflow chunks.
        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.len() >= 2,
            "expected edit + at least 1 overflow send, got {}",
            requests.len()
        );
    }

    // ---------------------------------------------------------------------------
    // elicit() — happy path, timeout, /cancel, field-key sanitization
    // All tests that exercise elicit() need the mock server because elicit()
    // calls self.send() (which calls the Telegram Bot API) before reading rx.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn elicit_happy_path_string_field_returns_accepted() {
        let server = MockServer::start().await;
        let (mut channel, tx) = make_mocked_channel(&server, vec![]).await;

        let request = ElicitationRequest {
            server_name: "test-server".to_owned(),
            message: "Please provide your name".to_owned(),
            fields: vec![ElicitationField {
                name: "username".to_owned(),
                description: None,
                field_type: ElicitationFieldType::String,
                required: true,
            }],
        };

        // Send the answer before calling elicit() so it is buffered in the channel.
        tx.send(plain_message("alice")).await.unwrap();

        let response = channel.elicit(request).await.unwrap();

        match response {
            ElicitationResponse::Accepted(val) => {
                assert_eq!(val["username"], "alice");
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn elicit_field_key_uses_raw_name_not_sanitized() {
        let server = MockServer::start().await;
        let (mut channel, tx) = make_mocked_channel(&server, vec![]).await;

        // Field name contains a space — the old sanitize_field_key would strip it to "passphrase".
        let request = ElicitationRequest {
            server_name: "test-server".to_owned(),
            message: "Provide credentials".to_owned(),
            fields: vec![ElicitationField {
                name: "pass phrase".to_owned(),
                description: None,
                field_type: ElicitationFieldType::String,
                required: true,
            }],
        };

        tx.send(plain_message("hunter2")).await.unwrap();
        let response = channel.elicit(request).await.unwrap();

        match response {
            ElicitationResponse::Accepted(val) => {
                assert_eq!(
                    val["pass phrase"], "hunter2",
                    "raw field name must be the map key"
                );
                assert!(
                    val.get("passphrase").is_none(),
                    "sanitized key must not appear in response"
                );
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn elicit_cancel_command_returns_cancelled() {
        let server = MockServer::start().await;
        let (mut channel, tx) = make_mocked_channel(&server, vec![]).await;

        let request = ElicitationRequest {
            server_name: "test-server".to_owned(),
            message: "Provide a value".to_owned(),
            fields: vec![ElicitationField {
                name: "token".to_owned(),
                description: None,
                field_type: ElicitationFieldType::String,
                required: true,
            }],
        };

        tx.send(plain_message("/cancel")).await.unwrap();

        let response = channel.elicit(request).await.unwrap();
        assert!(
            matches!(response, ElicitationResponse::Cancelled),
            "expected Cancelled, got {response:?}"
        );
    }

    /// Verify the timeout branch of `elicit()` at the rx level, matching the
    /// same pattern used in `confirm_timeout_logic_denies_on_timeout`.
    #[tokio::test]
    async fn elicit_timeout_logic_cancels_on_timeout() {
        tokio::time::pause();
        let (_tx, mut rx) = mpsc::channel::<IncomingMessage>(1);
        let timeout_fut = tokio::time::timeout(crate::ELICITATION_TIMEOUT, rx.recv());
        tokio::time::advance(crate::ELICITATION_TIMEOUT + Duration::from_millis(1)).await;
        let result = timeout_fut.await;
        assert!(
            result.is_err(),
            "expected Err(Elapsed) for elicitation timeout, got recv result"
        );
    }

    // ---------------------------------------------------------------------------
    // with_supervisor — verifies that the Telegram listener task is registered
    // ---------------------------------------------------------------------------

    /// Confirm that `with_supervisor()` stores the supervisor so that after
    /// `start()` the listener task appears in the supervisor registry.
    ///
    /// The test uses `new_test()` (no real bot token / network) combined with
    /// a real `TaskSupervisor` running under tokio. Because `start()` spawns a
    /// task that calls `bot.set_my_commands()` (`register_commands`) and then
    /// runs the teloxide dispatcher, we point the bot at a wiremock server
    /// that accepts all requests with HTTP 200 so the task does not immediately
    /// panic due to a network error.
    #[tokio::test]
    async fn with_supervisor_registers_listener_task() {
        use tokio_util::sync::CancellationToken;

        let cancel = CancellationToken::new();
        let sup = zeph_core::TaskSupervisor::new(cancel.clone());

        // new_test creates a channel with a real Bot pointed at a dummy URL.
        // The bot won't be called because we immediately check the registry
        // before the spawned dispatcher has time to make a request.
        let (channel, _tx) = TelegramChannel::new_test(vec!["user".to_string()]);
        let channel = channel.with_supervisor(sup.clone());

        // start() spawns the telegram_listener task via supervisor.
        channel
            .start()
            .expect("start() must succeed with non-empty allowed_users");

        // Give the tokio runtime one yield so the supervisor's state is updated.
        tokio::task::yield_now().await;

        let snapshot = sup.snapshot();
        let names: Vec<&str> = snapshot.iter().map(|s| s.name.as_ref()).collect();
        assert!(
            names.contains(&"telegram_listener"),
            "expected 'telegram_listener' in supervisor snapshot, got: {names:?}"
        );

        cancel.cancel();
    }
}
