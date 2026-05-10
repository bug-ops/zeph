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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::markdown::markdown_to_telegram;
use crate::telegram_api_ext::{BotAccessSettings, GuestMessage, TelegramApiClient};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, ChatAction, MessageId, ParseMode};
use tokio::sync::mpsc;
use zeph_common::TaskSupervisor;
use zeph_core::channel::{
    Attachment, AttachmentKind, Channel, ChannelError, ChannelMessage, ElicitationField,
    ElicitationFieldType, ElicitationRequest, ElicitationResponse,
};

const MAX_MESSAGE_LEN: usize = 4096;
const MAX_IMAGE_BYTES: u32 = 20 * 1024 * 1024;
const MAX_TRACKED_CHATS: usize = 1000;

type BotReplyCounters = Arc<Mutex<HashMap<ChatId, u32>>>;

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
///     .with_stream_interval(std::time::Duration::from_millis(2000))
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
    /// Minimum interval between streaming message edits. Defaults to 3 seconds.
    stream_interval: Duration,
    /// Raw Bot API 10.0 client for methods not yet covered by teloxide.
    api_ext: TelegramApiClient,
    /// Enable Guest Mode: respond to @mentions via `answerGuestQuery`.
    guest_mode: bool,
    /// Active guest query ID for the current response cycle.
    guest_query_id: Option<String>,
    /// Enable bot-to-bot communication.
    bot_to_bot: bool,
    /// Set to `true` by the background task when `setManagedBotAccessSettings` succeeds.
    /// Remains `false` if the API call fails, disabling bot-to-bot processing.
    bot_to_bot_active: Arc<AtomicBool>,
    /// Allowlist of bot usernames permitted when `bot_to_bot = true`. Empty = all bots.
    allowed_bots: Vec<String>,
    /// Maximum consecutive bot replies per chat before dropping.
    max_bot_chain_depth: u32,
    /// Per-chat consecutive bot reply counters for loop prevention.
    bot_reply_counters: BotReplyCounters,
    /// Optional supervisor used to register the Telegram listener task in the
    /// workspace-wide task registry with automatic restart on panic.
    supervisor: Option<TaskSupervisor>,
    /// Handle to the guest-mode axum proxy task. Kept alive for the lifetime
    /// of the channel; dropped when `TelegramChannel` is dropped.
    #[allow(dead_code)]
    guest_proxy_handle: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for TelegramChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramChannel")
            .field("chat_id", &self.chat_id)
            .field("allowed_users", &self.allowed_users)
            .field("accumulated_len", &self.accumulated.len())
            .field("stream_interval_ms", &self.stream_interval.as_millis())
            .field("guest_mode", &self.guest_mode)
            .field("bot_to_bot", &self.bot_to_bot)
            .field(
                "bot_to_bot_active",
                &self.bot_to_bot_active.load(Ordering::Relaxed),
            )
            .field("allowed_bots_count", &self.allowed_bots.len())
            .field("max_bot_chain_depth", &self.max_bot_chain_depth)
            .field("supervisor", &self.supervisor.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct IncomingMessage {
    chat_id: ChatId,
    text: String,
    attachments: Vec<Attachment>,
    /// `Some(query_id)` when this message came from a `guest_message` update.
    guest_query_id: Option<String>,
    /// `true` when the sender is a Telegram bot (`from.is_bot = true`).
    is_from_bot: bool,
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
    /// Use [`with_stream_interval`] to change the default 3-second streaming edit interval.
    ///
    /// [`start`]: TelegramChannel::start
    /// [`with_stream_interval`]: TelegramChannel::with_stream_interval
    #[must_use]
    pub fn new(token: impl Into<String>, allowed_users: Vec<String>) -> Self {
        let token = token.into();
        let bot = Bot::new(&token);
        let api_ext = TelegramApiClient::new(&token);
        let (_, rx) = mpsc::channel(64);
        Self {
            bot,
            chat_id: None,
            rx,
            allowed_users,
            accumulated: String::new(),
            last_edit: None,
            message_id: None,
            stream_interval: Duration::from_secs(3),
            api_ext,
            guest_mode: false,
            guest_query_id: None,
            bot_to_bot: false,
            bot_to_bot_active: Arc::new(AtomicBool::new(false)),
            allowed_bots: Vec::new(),
            max_bot_chain_depth: 1,
            bot_reply_counters: Arc::new(Mutex::new(HashMap::new())),
            supervisor: None,
            guest_proxy_handle: None,
        }
    }

    /// Set the minimum interval between streaming message edits.
    ///
    /// Values below 500 ms are clamped to 500 ms with a warning to prevent
    /// triggering Telegram's rate limits (30 edits/second per chat).
    ///
    /// Default: 3000 ms.
    #[must_use]
    pub fn with_stream_interval(mut self, interval: Duration) -> Self {
        const MIN_INTERVAL: Duration = Duration::from_millis(500);
        if interval < MIN_INTERVAL {
            tracing::warn!(
                requested_ms = interval.as_millis(),
                clamped_ms = MIN_INTERVAL.as_millis(),
                "stream_interval_ms is below the minimum safe value; clamping to 500ms to avoid Telegram rate limits"
            );
            self.stream_interval = MIN_INTERVAL;
        } else {
            self.stream_interval = interval;
        }
        self
    }

    /// Access the raw Bot API 10.0 client for methods not covered by teloxide.
    #[must_use]
    pub fn api_ext(&self) -> &TelegramApiClient {
        &self.api_ext
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

    /// Enable Bot API 10.0 Guest Mode — respond to @mentions via `answerGuestQuery`.
    ///
    /// When enabled, an HTTP proxy is started on loopback that intercepts `getUpdates`
    /// responses to extract `guest_message` updates before teloxide deserializes them.
    #[must_use]
    pub fn with_guest_mode(mut self, enabled: bool) -> Self {
        self.guest_mode = enabled;
        self
    }

    /// Enable bot-to-bot communication (Bot API 10.0).
    ///
    /// * `enabled` — whether to accept messages from other bots.
    /// * `allowed` — bot usernames (with `@` prefix) allowed to interact. Empty = all bots.
    /// * `max_depth` — maximum consecutive bot replies before dropping to prevent loops.
    #[must_use]
    pub fn with_bot_to_bot(mut self, enabled: bool, allowed: Vec<String>, max_depth: u32) -> Self {
        self.bot_to_bot = enabled;
        self.allowed_bots = allowed;
        self.max_bot_chain_depth = max_depth;
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
    #[allow(clippy::too_many_lines)]
    pub fn start(mut self) -> Result<Self, ChannelError> {
        if self.allowed_users.is_empty() {
            tracing::error!("telegram.allowed_users is empty; refusing to start an open bot");
            return Err(ChannelError::Other(
                "telegram.allowed_users must not be empty".into(),
            ));
        }

        // Bot-to-Bot: register capability with Telegram (non-fatal, fire-and-forget).
        // bot_to_bot_active starts as false; the spawned task sets it to true only on success.
        // If setManagedBotAccessSettings fails, bot messages are silently dropped (spec FR-003).
        if self.bot_to_bot {
            let api_ext = self.api_ext.clone();
            let active_flag = self.bot_to_bot_active.clone();
            tokio::spawn(async move {
                let settings = BotAccessSettings {
                    allow_user_messages: true,
                    allow_bot_messages: true,
                };
                match api_ext.set_managed_bot_access_settings(&settings).await {
                    Ok(_) => {
                        active_flag.store(true, Ordering::Release);
                        tracing::info!(
                            "bot-to-bot communication enabled via setManagedBotAccessSettings"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "setManagedBotAccessSettings failed: {e}; bot-to-bot disabled"
                        );
                    }
                }
            });
        }

        let (tx, rx) = mpsc::channel::<IncomingMessage>(64);
        self.rx = rx;

        // Guest Mode: spawn a transparent axum proxy on a local ephemeral port.
        // The proxy intercepts `getUpdates` responses to extract `guest_message` entries
        // before teloxide deserializes them (teloxide-core 0.13 discards unknown update kinds).
        // Redirecting `Bot` to the proxy via `set_api_url` avoids a second `getUpdates`
        // connection — no 409 Conflict risk (C3 fix).
        if self.guest_mode {
            let token = self.bot.token().to_owned();
            let (proxy_url, proxy_handle) =
                spawn_guest_proxy(&token, tx.clone(), self.allowed_users.clone())?;
            let proxy_url = reqwest::Url::parse(&proxy_url)
                .map_err(|e| ChannelError::Other(format!("guest proxy URL parse error: {e}")))?;
            self.bot = self.bot.clone().set_api_url(proxy_url);
            self.guest_proxy_handle = Some(proxy_handle);
        }

        let bot = self.bot.clone();
        let allowed = self.allowed_users.clone();
        let bot_to_bot = self.bot_to_bot;
        let bot_to_bot_active = self.bot_to_bot_active.clone();
        let allowed_bots = self.allowed_bots.clone();
        let max_bot_chain_depth = self.max_bot_chain_depth;
        let bot_reply_counters = self.bot_reply_counters.clone();

        Self::register_commands(bot.clone());

        let bot_for_factory = bot.clone();
        let allowed_for_factory = allowed.clone();
        let tx_for_factory = tx.clone();
        let bot_to_bot_active_for_factory = bot_to_bot_active.clone();
        let listener_factory = move || {
            let bot = bot_for_factory.clone();
            let allowed = allowed_for_factory.clone();
            let tx = tx_for_factory.clone();
            let bot_to_bot = bot_to_bot;
            let bot_to_bot_active = bot_to_bot_active_for_factory.clone();
            let allowed_bots = allowed_bots.clone();
            let max_bot_chain_depth = max_bot_chain_depth;
            let bot_reply_counters = bot_reply_counters.clone();
            async move {
                let handler = Update::filter_message().endpoint(move |msg: Message, bot: Bot| {
                    let tx = tx.clone();
                    let allowed = allowed.clone();
                    let allowed_bots = allowed_bots.clone();
                    let bot_reply_counters = bot_reply_counters.clone();
                    let bot_to_bot_active = bot_to_bot_active.clone();
                    async move {
                        handle_telegram_message(
                            bot,
                            msg,
                            tx,
                            allowed,
                            bot_to_bot,
                            bot_to_bot_active,
                            allowed_bots,
                            max_bot_chain_depth,
                            bot_reply_counters,
                        )
                        .await
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
            sup.spawn(zeph_common::TaskDescriptor {
                name: "telegram_listener",
                restart: zeph_common::RestartPolicy::Restart {
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
            stream_interval: Duration::from_secs(3),
            api_ext: TelegramApiClient::new("test_token"),
            guest_mode: false,
            guest_query_id: None,
            bot_to_bot: false,
            bot_to_bot_active: Arc::new(AtomicBool::new(false)),
            allowed_bots: Vec::new(),
            max_bot_chain_depth: 1,
            bot_reply_counters: Arc::new(Mutex::new(HashMap::new())),
            supervisor: None,
            guest_proxy_handle: None,
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
            Some(last) => last.elapsed() > self.stream_interval,
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
                        .map_err(ChannelError::telegram)?;
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
                                .map_err(ChannelError::telegram)?;
                            self.message_id = Some(msg.id);
                        } else {
                            return Err(ChannelError::telegram(e));
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
                            .map_err(ChannelError::telegram)?;
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

/// Returns `true` when `username` appears in `allowed`.
///
/// An empty `allowed` list is treated as "no restriction" and always returns
/// `true`, but `start()` rejects empty lists before the listener is spawned,
/// so in practice `allowed` is never empty at call time.
fn is_user_authorized(username: Option<&str>, allowed: &[String]) -> bool {
    allowed.is_empty() || username.is_some_and(|u| allowed.iter().any(|a| a == u))
}

/// Returns `true` when `username` is in `allowed_bots` or the list is empty.
fn is_bot_authorized(username: Option<&str>, allowed_bots: &[String]) -> bool {
    allowed_bots.is_empty()
        || username.is_some_and(|u| {
            allowed_bots.iter().any(|a| {
                // Compare with or without "@" prefix
                a.trim_start_matches('@') == u.trim_start_matches('@')
            })
        })
}

/// Increments the consecutive bot-reply counter for `chat_id` and returns the new depth.
///
/// Evicts one arbitrary entry when at capacity to bound memory usage.
fn increment_bot_depth(counters: &BotReplyCounters, chat_id: ChatId) -> u32 {
    let mut map = counters.lock().expect("bot_reply_counters poisoned");
    if map.len() >= MAX_TRACKED_CHATS && !map.contains_key(&chat_id) {
        // Evict one arbitrary entry to bound memory. HashMap has no insertion order,
        // so we evict the first key returned by the iterator (effectively random).
        // This prevents a flood of unique chat IDs from resetting all legitimate counters.
        if let Some(&evict_key) = map.keys().next() {
            map.remove(&evict_key);
        }
    }
    let counter = map.entry(chat_id).or_insert(0);
    *counter += 1;
    *counter
}

/// Resets the bot-reply counter for `chat_id` on a non-bot message.
fn reset_bot_depth(counters: &BotReplyCounters, chat_id: ChatId) {
    counters
        .lock()
        .expect("bot_reply_counters poisoned")
        .remove(&chat_id);
}

/// Shared state for the guest-mode axum proxy.
#[derive(Clone)]
struct GuestProxyState {
    upstream: String,
    client: reqwest::Client,
    tx: mpsc::Sender<IncomingMessage>,
    allowed_users: Vec<String>,
}

/// Guest Mode transparent HTTP proxy.
///
/// Binds an ephemeral TCP port on `127.0.0.1`, proxies all requests from the
/// teloxide `Bot` to `https://api.telegram.org`. For `getUpdates` requests the
/// proxy injects `"guest_message"` into `allowed_updates` so Telegram delivers
/// these updates, extracts copies of `guest_message` entries from the response
/// to forward them to the agent, and returns the **original unmodified** response
/// to teloxide so its internal offset advances correctly — preventing duplicates.
/// This guarantees a single `getUpdates` connection per bot token — no 409 Conflict.
///
/// Returns the local URL (`http://127.0.0.1:<port>`) to pass to
/// `Bot::set_api_url()`, and a [`tokio::task::JoinHandle`] for the proxy task.
fn spawn_guest_proxy(
    token: &str,
    tx: mpsc::Sender<IncomingMessage>,
    allowed_users: Vec<String>,
) -> Result<(String, tokio::task::JoinHandle<()>), ChannelError> {
    let upstream = format!("https://api.telegram.org/bot{token}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_mins(1))
        .build()
        .map_err(|e| ChannelError::Other(format!("guest proxy client init failed: {e}")))?;

    let state = GuestProxyState {
        upstream,
        client,
        tx,
        allowed_users,
    };

    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| ChannelError::Other(format!("guest proxy bind failed: {e}")))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| ChannelError::Other(format!("guest proxy addr failed: {e}")))?;
    let local_url = format!("http://127.0.0.1:{}/bot{token}", local_addr.port());

    let app = Router::new()
        .route("/{*path}", any(proxy_handler))
        .with_state(state);

    let handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener)
            .expect("guest proxy: TcpListener conversion failed");
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!("guest proxy axum serve error: {e}");
        }
    });

    Ok((local_url, handle))
}

/// Axum handler: proxies one request upstream and, on `getUpdates`, extracts
/// `guest_message` entries from the Telegram API response.
///
/// For `getUpdates` requests the handler:
/// 1. Injects `"guest_message"` into the outgoing `allowed_updates` array so
///    Telegram actually delivers these updates (teloxide omits them by default).
/// 2. Extracts a copy of any `guest_message` entries from the response and
///    forwards them to the agent via `tx`.
/// 3. Returns the **full** unmodified result array to teloxide — including the
///    original `guest_message` objects — so teloxide's internal offset advances
///    correctly and the same updates are never replayed.  Teloxide's
///    `Update::filter_message()` handler simply ignores unknown `UpdateKind`
///    variants, so the extra entries are silently dropped by the dispatcher.
async fn proxy_handler(State(state): State<GuestProxyState>, req: Request<Body>) -> Response {
    // Path structure from teloxide: /bot<TOKEN>/<method>
    // proxy_url passed to Bot::set_api_url is http://127.0.0.1:<port>/bot<TOKEN>
    // teloxide appends /<method> to base_url, so path is /bot<TOKEN>/<method>
    let path = req.uri().path();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let method_part = path.splitn(4, '/').nth(3).unwrap_or("");
    let upstream_url = format!("{}/{method_part}{query}", state.upstream);

    let is_get_updates = method_part == "getUpdates";

    let method = req.method().clone();
    let headers = req.headers().clone();
    let mut body_bytes = match axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("guest proxy: failed to read request body: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    // Inject "guest_message" into allowed_updates so Telegram delivers these updates.
    // teloxide only requests ["message"] by default; without this injection
    // Telegram never sends guest_message entries and Guest Mode is silently broken.
    if is_get_updates
        && let Ok(mut body_json) = serde_json::from_slice::<serde_json::Value>(&body_bytes)
    {
        let injected = serde_json::json!("guest_message");
        match body_json
            .get_mut("allowed_updates")
            .and_then(|v| v.as_array_mut())
        {
            Some(arr) => {
                if !arr.iter().any(|v| v.as_str() == Some("guest_message")) {
                    arr.push(injected);
                }
            }
            None => {
                body_json["allowed_updates"] = serde_json::json!(["message", "guest_message"]);
            }
        }
        if let Ok(b) = serde_json::to_vec(&body_json) {
            body_bytes = b.into();
        }
    }

    let mut upstream_req = state.client.request(method, &upstream_url);
    for (name, value) in &headers {
        upstream_req = upstream_req.header(name, value);
    }
    upstream_req = upstream_req.body(body_bytes);

    let upstream_resp = match upstream_req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("guest proxy upstream error: {}", e.without_url());
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();
    let resp_bytes = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                "guest proxy: failed to read upstream response: {}",
                e.without_url()
            );
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    // On getUpdates success: extract guest_message entries and forward copies to the
    // agent, then return the original response unchanged to teloxide so its offset
    // advances correctly (no duplicate delivery on the next poll).
    if is_get_updates
        && status.is_success()
        && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&resp_bytes)
        && let Some(result) = json.get("result").and_then(|r| r.as_array())
    {
        for update in result {
            if let Some(gm_val) = update.get("guest_message") {
                extract_and_forward_guest_message(gm_val, &state.tx, &state.allowed_users).await;
            }
        }
    }

    let mut response = Response::new(Body::from(resp_bytes));
    *response.status_mut() = status;
    for (name, value) in &resp_headers {
        response.headers_mut().insert(name, value.clone());
    }
    response
}

/// Parse a raw `guest_message` JSON value and forward it to the agent.
async fn extract_and_forward_guest_message(
    gm_val: &serde_json::Value,
    tx: &mpsc::Sender<IncomingMessage>,
    allowed_users: &[String],
) {
    let gm: GuestMessage = match serde_json::from_value(gm_val.clone()) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("failed to parse guest_message: {e}");
            return;
        }
    };

    let caller_username = gm.guest_bot_caller_user.username.as_deref();
    if !is_user_authorized(caller_username, allowed_users) {
        tracing::warn!(
            username = ?caller_username,
            "guest_message from unauthorized user — dropped"
        );
        return;
    }

    let text = gm.text.unwrap_or_default();
    let chat_id = ChatId(gm.guest_bot_caller_chat.id);
    let _ = tx
        .send(IncomingMessage {
            chat_id,
            text,
            attachments: Vec::new(),
            guest_query_id: Some(gm.guest_query_id),
            is_from_bot: gm.guest_bot_caller_user.is_bot,
        })
        .await;
}

/// Extract the audio file identifier and byte size from a message, if present.
///
/// Prefers a voice note over an audio file when both are present.
fn extract_audio_attachment(msg: &Message) -> Option<(String, u32)> {
    msg.voice()
        .map(|v| (v.file.id.0.clone(), v.file.size))
        .or_else(|| msg.audio().map(|a| (a.file.id.0.clone(), a.file.size)))
}

/// Extract the largest photo's file identifier and byte size from a message.
///
/// Returns `None` when the message contains no photo or the largest available
/// size exceeds [`MAX_IMAGE_BYTES`].
fn extract_photo_attachment(msg: &Message) -> Option<(String, u32)> {
    let photos = msg.photo()?;
    let photo = photos.iter().max_by_key(|p| p.file.size)?;
    if photo.file.size > MAX_IMAGE_BYTES {
        tracing::warn!(
            size = photo.file.size,
            max = MAX_IMAGE_BYTES,
            "photo exceeds size limit, skipping"
        );
        return None;
    }
    Some((photo.file.id.0.clone(), photo.file.size))
}

/// Walk `reply_to_message` links to compute structural chain depth (spec FR-007).
///
/// Returns the number of `reply_to_message` hops from `msg` up to and including
/// the root, capped at `cap + 1` to bound recursion.  The Telegram API only
/// exposes one level of nesting in the payload, so in practice this returns 0
/// (no reply) or 1 (direct reply); the cap is a safeguard against future changes.
fn compute_chain_depth(msg: &Message, cap: u32) -> u32 {
    let mut depth = 0u32;
    let mut current = msg.reply_to_message();
    while current.is_some() && depth < cap + 1 {
        depth += 1;
        current = current.and_then(|m| m.reply_to_message());
    }
    depth
}

/// Process one incoming Telegram update inside the dispatcher endpoint.
///
/// Checks authorization, applies bot-to-bot policy (structural chain depth via
/// [`compute_chain_depth`] + consecutive-reply counter), downloads any media
/// attachments, and forwards the assembled [`IncomingMessage`] to the agent
/// via `tx`.  Returns `respond(())` in all branches so teloxide considers the
/// update handled.
#[allow(clippy::too_many_arguments)]
async fn handle_telegram_message(
    bot: Bot,
    msg: Message,
    tx: mpsc::Sender<IncomingMessage>,
    allowed: Vec<String>,
    bot_to_bot: bool,
    bot_to_bot_active: Arc<AtomicBool>,
    allowed_bots: Vec<String>,
    max_bot_chain_depth: u32,
    bot_reply_counters: BotReplyCounters,
) -> Result<(), teloxide::RequestError> {
    let sender_is_bot = msg.from.as_ref().is_some_and(|u| u.is_bot);

    if sender_is_bot {
        if !bot_to_bot || !bot_to_bot_active.load(Ordering::Acquire) {
            // Feature disabled — silently drop without logging
            return respond(());
        }
        let sender_username = msg.from.as_ref().and_then(|u| u.username.as_deref());
        if !is_bot_authorized(sender_username, &allowed_bots) {
            tracing::warn!(sender = ?sender_username, "rejected message from unauthorized bot");
            return respond(());
        }
        // Primary check: structural reply chain depth (spec FR-007).
        // Telegram API only exposes 1 level, so this catches direct reply loops.
        let chain_depth = compute_chain_depth(&msg, max_bot_chain_depth);
        if chain_depth >= max_bot_chain_depth {
            tracing::warn!(
                chain_depth,
                max = max_bot_chain_depth,
                message_id = msg.id.0,
                sender = ?sender_username,
                "dropping bot message: reply chain depth limit reached"
            );
            return respond(());
        }
        // Secondary check: consecutive bot replies in this chat (defense-in-depth).
        let consec_depth = increment_bot_depth(&bot_reply_counters, msg.chat.id);
        if consec_depth >= max_bot_chain_depth {
            tracing::warn!(
                consec_depth,
                max = max_bot_chain_depth,
                message_id = msg.id.0,
                sender = ?sender_username,
                "dropping bot message: consecutive reply limit reached"
            );
            return respond(());
        }
    } else {
        // Non-bot message: reset consecutive bot reply counter for this chat.
        reset_bot_depth(&bot_reply_counters, msg.chat.id);

        let username = msg.from.as_ref().and_then(|u| u.username.as_deref());
        if !is_user_authorized(username, &allowed) {
            tracing::warn!("rejected message from unauthorized user: {:?}", username);
            return respond(());
        }
    }

    let text = msg.text().unwrap_or_default().to_string();
    let mut attachments = Vec::new();

    if let Some((file_id, file_size)) = extract_audio_attachment(&msg) {
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

    if let Some((file_id, file_size)) = extract_photo_attachment(&msg) {
        match download_file(&bot, file_id, file_size).await {
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

    if text.is_empty() && attachments.is_empty() {
        return respond(());
    }

    let _ = tx
        .send(IncomingMessage {
            chat_id: msg.chat.id,
            text,
            attachments,
            guest_query_id: None,
            is_from_bot: sender_is_bot,
        })
        .await;

    respond(())
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
            let is_guest = incoming.guest_query_id.is_some();
            self.guest_query_id.clone_from(&incoming.guest_query_id);
            ChannelMessage {
                text: incoming.text,
                attachments: incoming.attachments,
                is_guest_context: is_guest,
                is_from_bot: incoming.is_from_bot,
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

            let is_guest = incoming.guest_query_id.is_some();
            self.guest_query_id.clone_from(&incoming.guest_query_id);

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
                            is_guest_context: false,
                            is_from_bot: false,
                        }));
                    }
                    "/skills" => {
                        return Ok(Some(ChannelMessage {
                            text: "/skills".to_string(),
                            attachments: vec![],
                            is_guest_context: false,
                            is_from_bot: false,
                        }));
                    }
                    _ => {}
                }
            }

            return Ok(Some(ChannelMessage {
                text: incoming.text,
                attachments: incoming.attachments,
                is_guest_context: is_guest,
                is_from_bot: incoming.is_from_bot,
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
    /// `Err(ChannelError::Telegram)` if the Telegram API call fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "channel.telegram.send", skip_all, fields(msg_len = %text.len()))
    )]
    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        // Guest context: accumulate full response — flush_chunks calls answerGuestQuery once.
        if self.guest_query_id.is_some() {
            if !self.accumulated.is_empty() {
                self.accumulated.push('\n');
            }
            self.accumulated.push_str(text);
            return Ok(());
        }

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
                .map_err(ChannelError::telegram)?;
        } else {
            let chunks = crate::markdown::utf8_chunks(&formatted_text, MAX_MESSAGE_LEN);
            for chunk in chunks {
                self.bot
                    .send_message(chat_id, chunk)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await
                    .map_err(ChannelError::telegram)?;
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

        // In guest context, accumulate only — never call send_or_edit (NFR-005).
        // The full accumulated text is sent via answerGuestQuery in flush_chunks.
        if self.guest_query_id.is_some() {
            return Ok(());
        }

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

        // Guest context: send accumulated text via answerGuestQuery (single call).
        if let Some(query_id) = self.guest_query_id.take() {
            let full_text = std::mem::take(&mut self.accumulated);
            let text = full_text.trim();
            if text.len() > MAX_MESSAGE_LEN {
                tracing::warn!(
                    query_id,
                    bytes = text.len(),
                    max = MAX_MESSAGE_LEN,
                    "guest response exceeds 4096 bytes; Telegram truncates answerGuestQuery — consider shorter responses"
                );
            }
            if !text.is_empty()
                && let Err(e) = self
                    .api_ext
                    .answer_guest_query(&query_id, text, Some("HTML"))
                    .await
            {
                tracing::warn!(query_id, "answer_guest_query failed: {e}");
            }
            self.last_edit = None;
            self.message_id = None;
            return Ok(());
        }

        // Send if there is unsent accumulated text OR an existing message to finalize.
        // The `message_id.is_some()` guard alone would silently discard text that arrived
        // before the first interval elapsed (so `send_or_edit` was never called).
        if self.message_id.is_some() || !self.accumulated.is_empty() {
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
            .map_err(ChannelError::telegram)?;
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
            stream_interval: Duration::from_secs(3),
            api_ext: TelegramApiClient::with_base_url(server.uri()),
            guest_mode: false,
            guest_query_id: None,
            bot_to_bot: false,
            bot_to_bot_active: Arc::new(AtomicBool::new(false)),
            allowed_bots: Vec::new(),
            max_bot_chain_depth: 1,
            bot_reply_counters: Arc::new(Mutex::new(HashMap::new())),
            supervisor: None,
            guest_proxy_handle: None,
        };
        (channel, tx)
    }

    fn plain_message(text: &str) -> IncomingMessage {
        IncomingMessage {
            chat_id: ChatId(1),
            text: text.to_string(),
            attachments: vec![],
            guest_query_id: None,
            is_from_bot: false,
        }
    }

    // ---------------------------------------------------------------------------
    // Pure-function unit tests (no async, no network)
    // ---------------------------------------------------------------------------

    #[test]
    fn is_user_authorized_empty_allowed_permits_all() {
        assert!(is_user_authorized(None, &[]));
        assert!(is_user_authorized(Some("anyone"), &[]));
    }

    #[test]
    fn is_user_authorized_known_user_is_permitted() {
        let allowed = vec!["alice".to_string(), "bob".to_string()];
        assert!(is_user_authorized(Some("alice"), &allowed));
        assert!(is_user_authorized(Some("bob"), &allowed));
    }

    #[test]
    fn is_user_authorized_unknown_user_is_rejected() {
        let allowed = vec!["alice".to_string()];
        assert!(!is_user_authorized(Some("eve"), &allowed));
        assert!(!is_user_authorized(None, &allowed));
    }

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
    fn with_stream_interval_custom_interval_respected() {
        let mut channel = TelegramChannel::new("test_token".to_string(), Vec::new())
            .with_stream_interval(Duration::from_secs(2));
        // 1500ms elapsed < 2000ms interval — should NOT send
        channel.last_edit = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(1500))
                .unwrap(),
        );
        assert!(!channel.should_send_update());
        // 2500ms elapsed > 2000ms interval — should send
        channel.last_edit = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(2500))
                .unwrap(),
        );
        assert!(channel.should_send_update());
    }

    #[test]
    fn with_stream_interval_clamps_below_500ms() {
        let channel = TelegramChannel::new("test_token".to_string(), Vec::new())
            .with_stream_interval(Duration::from_millis(100));
        assert_eq!(channel.stream_interval, Duration::from_millis(500));
    }

    #[test]
    fn with_stream_interval_default_is_3s() {
        let channel = TelegramChannel::new("test_token", Vec::new());
        assert_eq!(channel.stream_interval, Duration::from_secs(3));
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
    async fn flush_chunks_clears_state_when_no_accumulated_and_no_message_id() {
        let (mut channel, _tx) = TelegramChannel::new_test(vec![]);
        // accumulated is empty and message_id is None — no API call expected.
        channel.last_edit = Some(Instant::now());

        channel.flush_chunks().await.unwrap();

        assert!(channel.accumulated.is_empty());
        assert!(channel.last_edit.is_none());
        assert!(channel.message_id.is_none());
    }

    // flush_chunks() must send accumulated text even when message_id is None.
    // Regression test for the data-loss bug where a short streaming response
    // (entire reply arrives before stream_interval elapses) was silently discarded.
    #[tokio::test]
    async fn flush_chunks_sends_when_accumulated_but_message_id_is_none() {
        let server = MockServer::start().await;
        let (mut channel, _tx) = make_mocked_channel(&server, vec![]).await;

        // Suppress the periodic send by setting last_edit to now (interval not elapsed).
        channel.last_edit = Some(Instant::now());
        channel.accumulated = "short reply".to_string();
        // message_id is None — the periodic path never fired.

        channel.flush_chunks().await.unwrap();

        // The mock server must have received exactly one sendMessage call.
        let requests = server.received_requests().await.unwrap();
        assert!(
            !requests.is_empty(),
            "flush_chunks must send accumulated text even when message_id is None"
        );
        assert!(channel.accumulated.is_empty());
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
    // is_bot_authorized — pure function tests
    // ---------------------------------------------------------------------------

    #[test]
    fn is_bot_authorized_empty_list_permits_all() {
        assert!(is_bot_authorized(None, &[]));
        assert!(is_bot_authorized(Some("@any_bot"), &[]));
    }

    #[test]
    fn is_bot_authorized_known_bot_permitted() {
        let allowed = vec!["@trusted".to_string()];
        assert!(is_bot_authorized(Some("@trusted"), &allowed));
        // Without @ prefix on the username side
        assert!(is_bot_authorized(Some("trusted"), &allowed));
    }

    #[test]
    fn is_bot_authorized_unknown_bot_rejected() {
        let allowed = vec!["@trusted".to_string()];
        assert!(!is_bot_authorized(Some("@evil"), &allowed));
        assert!(!is_bot_authorized(None, &allowed));
    }

    // ---------------------------------------------------------------------------
    // Bot depth counter — pure function tests
    // ---------------------------------------------------------------------------

    #[test]
    fn bot_depth_counter_increments_per_call() {
        let counters = BotReplyCounters::default();
        let chat = ChatId(1);
        assert_eq!(increment_bot_depth(&counters, chat), 1);
        assert_eq!(increment_bot_depth(&counters, chat), 2);
        assert_eq!(increment_bot_depth(&counters, chat), 3);
    }

    #[test]
    fn bot_depth_counter_resets_on_human_message() {
        let counters = BotReplyCounters::default();
        let chat = ChatId(42);
        increment_bot_depth(&counters, chat);
        increment_bot_depth(&counters, chat);
        reset_bot_depth(&counters, chat);
        assert_eq!(increment_bot_depth(&counters, chat), 1);
    }

    #[test]
    fn bot_depth_evicts_one_entry_when_at_capacity() {
        let counters: BotReplyCounters = Arc::new(Mutex::new(HashMap::new()));
        // Fill to capacity
        {
            let mut map = counters.lock().unwrap();
            for i in 0..1000_i64 {
                map.insert(ChatId(i), 1);
            }
        }
        // New chat should trigger single-entry eviction, not clear all
        let new_chat = ChatId(1001);
        increment_bot_depth(&counters, new_chat);
        let map = counters.lock().unwrap();
        // Total entries must remain <= MAX_TRACKED_CHATS (one evicted, one added)
        assert_eq!(map.len(), MAX_TRACKED_CHATS);
        // New entry must be present
        assert!(map.contains_key(&new_chat));
    }

    // ---------------------------------------------------------------------------
    // Guest context — recv() stores guest_query_id and sets is_guest_context
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn guest_context_stored_on_recv() {
        let (mut channel, tx) = TelegramChannel::new_test(vec!["alice".to_string()]);
        tx.send(IncomingMessage {
            chat_id: ChatId(1),
            text: "hello".to_string(),
            attachments: vec![],
            guest_query_id: Some("qid123".to_string()),
            is_from_bot: false,
        })
        .await
        .unwrap();
        let msg = channel.recv().await.unwrap().unwrap();
        assert!(msg.is_guest_context);
        assert_eq!(channel.guest_query_id.as_deref(), Some("qid123"));
    }

    #[tokio::test]
    async fn send_chunk_does_not_call_api_in_guest_context() {
        let (mut channel, _tx) = TelegramChannel::new_test(vec![]);
        channel.guest_query_id = Some("qid".to_string());
        // Simulate interval elapsed so send_or_edit would fire if not guarded
        channel.last_edit = None;

        channel.send_chunk("part1").await.unwrap();
        channel.send_chunk(" part2").await.unwrap();

        // Accumulated text must be present (guard ran, no API call was possible anyway
        // since there is no mock server — if send_or_edit were called, it would panic)
        assert_eq!(channel.accumulated, "part1 part2");
    }

    #[tokio::test]
    async fn flush_chunks_routes_to_answer_guest_query() {
        use wiremock::matchers::{method, path};

        let server = MockServer::start().await;

        // Mount a scoped mock specifically for answerGuestQuery.
        // SentGuestMessage deserializes {message_id: i64, chat_id: i64}.
        let answer_mock = Mock::given(method("POST"))
            .and(path("/answerGuestQuery"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {
                    "message_id": 1,
                    "chat_id": 2
                }
            })));
        let answer_handle = server.register_as_scoped(answer_mock).await;

        let (mut channel, _tx) = make_mocked_channel(&server, vec![]).await;
        channel.guest_query_id = Some("qid42".to_string());
        channel.accumulated = "response text".to_string();

        channel.flush_chunks().await.unwrap();

        assert_eq!(
            answer_handle.received_requests().await.len(),
            1,
            "answerGuestQuery must be called exactly once"
        );
        assert!(channel.guest_query_id.is_none());
        assert!(channel.accumulated.is_empty());
    }

    #[tokio::test]
    async fn bot_to_bot_false_drops_bot_messages() {
        // With bot_to_bot=false, messages from bots must be silently dropped.
        let (tx, mut rx) = mpsc::channel::<IncomingMessage>(8);
        let bot = Bot::new("test_token");
        let bot_reply_counters: BotReplyCounters = Arc::new(Mutex::new(HashMap::new()));
        let bot_to_bot_active = Arc::new(AtomicBool::new(false));

        let msg_json = serde_json::json!({
            "message_id": 1,
            "date": 1_700_000_000_i64,
            "chat": {"id": 1, "type": "private"},
            "from": {
                "id": 999,
                "is_bot": true,
                "first_name": "EvilBot",
                "username": "evil_bot"
            },
            "text": "bot says hi"
        });
        let msg: teloxide::types::Message = serde_json::from_value(msg_json).unwrap();

        handle_telegram_message(
            bot,
            msg,
            tx.clone(),
            vec!["human".to_string()],
            false, // bot_to_bot disabled
            bot_to_bot_active,
            vec![],
            3,
            bot_reply_counters,
        )
        .await
        .unwrap();

        drop(tx);
        // Channel must be empty: bot message was dropped before reaching tx.send().
        assert!(
            rx.recv().await.is_none(),
            "bot message must be dropped when bot_to_bot=false"
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
        let sup = zeph_common::TaskSupervisor::new(cancel.clone());

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
