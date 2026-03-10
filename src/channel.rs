// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_channels::AnyChannel;
use zeph_channels::CliChannel;
#[cfg(feature = "discord")]
use zeph_channels::discord::DiscordChannel;
#[cfg(feature = "slack")]
use zeph_channels::slack::SlackChannel;
use zeph_channels::telegram::TelegramChannel;
#[cfg(feature = "tui")]
use zeph_core::channel::{Channel, ChannelError, ChannelMessage, ToolOutputEvent};
use zeph_core::config::Config;
#[cfg(feature = "tui")]
use zeph_tui::TuiChannel;

pub(crate) type CliHistory = (Vec<String>, Box<dyn Fn(&str) + Send>);

#[cfg(feature = "tui")]
#[derive(Debug)]
pub(crate) enum AppChannel {
    Standard(AnyChannel),
    Tui(TuiChannel),
}

#[cfg(feature = "tui")]
macro_rules! dispatch_app_channel {
    ($self:expr, $method:ident $(, $arg:expr)*) => {
        match $self {
            AppChannel::Standard(c) => c.$method($($arg),*).await,
            AppChannel::Tui(c) => c.$method($($arg),*).await,
        }
    };
}

#[cfg(feature = "tui")]
impl Channel for AppChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        dispatch_app_channel!(self, recv)
    }
    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send, text)
    }
    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_chunk, chunk)
    }
    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, flush_chunks)
    }
    async fn send_typing(&mut self) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_typing)
    }
    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        dispatch_app_channel!(self, confirm, prompt)
    }
    fn try_recv(&mut self) -> Option<ChannelMessage> {
        match self {
            Self::Standard(c) => c.try_recv(),
            Self::Tui(c) => c.try_recv(),
        }
    }

    fn supports_exit(&self) -> bool {
        match self {
            Self::Standard(c) => c.supports_exit(),
            Self::Tui(c) => c.supports_exit(),
        }
    }
    async fn send_status(&mut self, text: &str) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_status, text)
    }
    async fn send_queue_count(&mut self, count: usize) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_queue_count, count)
    }
    async fn send_diff(&mut self, diff: zeph_core::DiffData) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_diff, diff)
    }
    async fn send_tool_output(&mut self, event: ToolOutputEvent<'_>) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_tool_output, event)
    }
}

#[cfg(feature = "tui")]
pub(crate) struct TuiHandle {
    pub(crate) user_tx: tokio::sync::mpsc::Sender<String>,
    pub(crate) agent_tx: tokio::sync::mpsc::Sender<zeph_tui::AgentEvent>,
    pub(crate) agent_rx: tokio::sync::mpsc::Receiver<zeph_tui::AgentEvent>,
    pub(crate) command_tx: tokio::sync::mpsc::Sender<zeph_tui::TuiCommand>,
    pub(crate) command_rx: tokio::sync::mpsc::Receiver<zeph_tui::TuiCommand>,
}

#[allow(clippy::unused_async)]
pub(crate) async fn create_channel_inner(
    config: &Config,
    history: Option<CliHistory>,
) -> anyhow::Result<AnyChannel> {
    #[cfg(feature = "discord")]
    if let Some(dc) = &config.discord
        && let Some(token) = &dc.token
    {
        let channel = DiscordChannel::new(
            token.clone(),
            dc.allowed_user_ids.clone(),
            dc.allowed_role_ids.clone(),
            dc.allowed_channel_ids.clone(),
        );
        tracing::info!("running in Discord mode");
        return Ok(AnyChannel::Discord(channel));
    }

    #[cfg(feature = "slack")]
    if let Some(sl) = &config.slack
        && let Some(bot_token) = &sl.bot_token
    {
        let signing_secret = sl.signing_secret.clone().unwrap_or_default();
        let channel = SlackChannel::new(
            bot_token.clone(),
            signing_secret,
            sl.webhook_host.clone(),
            sl.port,
            sl.allowed_user_ids.clone(),
            sl.allowed_channel_ids.clone(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        tracing::info!(
            "running in Slack mode (events on {}:{})",
            sl.webhook_host,
            sl.port
        );
        return Ok(AnyChannel::Slack(channel));
    }

    if let Some(token) = config.telegram.as_ref().and_then(|t| t.token.clone()) {
        let allowed = config
            .telegram
            .as_ref()
            .map_or_else(Vec::new, |t| t.allowed_users.clone());
        let tg = TelegramChannel::new(token, allowed).start()?;
        tracing::info!("running in Telegram mode");
        return Ok(AnyChannel::Telegram(tg));
    }

    if let Some((entries, persist_fn)) = history {
        let cli = CliChannel::with_history(entries, persist_fn);
        return Ok(AnyChannel::Cli(cli));
    }

    Ok(AnyChannel::Cli(CliChannel::new()))
}

#[cfg(feature = "tui")]
pub(crate) async fn create_channel_with_tui(
    config: &Config,
    tui_active: bool,
    history: Option<CliHistory>,
) -> anyhow::Result<(AppChannel, Option<TuiHandle>)> {
    if tui_active {
        let (user_tx, user_rx) = tokio::sync::mpsc::channel(32);
        let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(256);
        let agent_tx_clone = agent_tx.clone();
        // command_tx goes to App; command_rx is handled by forward_tui_commands task.
        let (command_tx, command_rx) = tokio::sync::mpsc::channel::<zeph_tui::TuiCommand>(16);
        let channel = TuiChannel::new(user_rx, agent_tx);
        let handle = TuiHandle {
            user_tx,
            agent_tx: agent_tx_clone,
            agent_rx,
            command_tx,
            command_rx,
        };
        return Ok((AppChannel::Tui(channel), Some(handle)));
    }
    let channel = create_channel_inner(config, history).await?;
    Ok((AppChannel::Standard(channel), None))
}

#[cfg(test)]
pub(crate) async fn create_channel(config: &Config) -> anyhow::Result<AnyChannel> {
    create_channel_inner(config, None).await
}

pub(crate) async fn build_cli_history(
    memory: &zeph_memory::semantic::SemanticMemory,
) -> Option<CliHistory> {
    let entries = memory
        .sqlite()
        .load_input_history(1000)
        .await
        .unwrap_or_default();
    let store = memory.sqlite().clone();
    let persist: Box<dyn Fn(&str) + Send> = Box::new(move |text: &str| {
        let store = store.clone();
        let text = text.to_owned();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = store.save_input_entry(&text).await {
                    tracing::warn!("failed to persist input history entry: {e}");
                }
            });
        }
    });
    Some((entries, persist))
}
