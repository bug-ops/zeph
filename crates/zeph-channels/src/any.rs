use zeph_core::channel::{Channel, ChannelError, ChannelMessage};

use crate::cli::CliChannel;
#[cfg(feature = "discord")]
use crate::discord::DiscordChannel;
#[cfg(feature = "slack")]
use crate::slack::SlackChannel;
use crate::telegram::TelegramChannel;

/// Enum dispatch for runtime channel selection.
#[derive(Debug)]
pub enum AnyChannel {
    Cli(CliChannel),
    Telegram(TelegramChannel),
    #[cfg(feature = "discord")]
    Discord(DiscordChannel),
    #[cfg(feature = "slack")]
    Slack(SlackChannel),
}

macro_rules! dispatch_channel {
    ($self:expr, $method:ident $(, $arg:expr)*) => {
        match $self {
            AnyChannel::Cli(c) => c.$method($($arg),*).await,
            AnyChannel::Telegram(c) => c.$method($($arg),*).await,
            #[cfg(feature = "discord")]
            AnyChannel::Discord(c) => c.$method($($arg),*).await,
            #[cfg(feature = "slack")]
            AnyChannel::Slack(c) => c.$method($($arg),*).await,
        }
    };
}

impl Channel for AnyChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        dispatch_channel!(self, recv)
    }

    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        dispatch_channel!(self, send, text)
    }

    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        dispatch_channel!(self, send_chunk, chunk)
    }

    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        dispatch_channel!(self, flush_chunks)
    }

    async fn send_typing(&mut self) -> Result<(), ChannelError> {
        dispatch_channel!(self, send_typing)
    }

    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        dispatch_channel!(self, confirm, prompt)
    }

    fn try_recv(&mut self) -> Option<ChannelMessage> {
        match self {
            Self::Cli(c) => c.try_recv(),
            Self::Telegram(c) => c.try_recv(),
            #[cfg(feature = "discord")]
            Self::Discord(c) => c.try_recv(),
            #[cfg(feature = "slack")]
            Self::Slack(c) => c.try_recv(),
        }
    }

    async fn send_status(&mut self, text: &str) -> Result<(), ChannelError> {
        dispatch_channel!(self, send_status, text)
    }

    async fn send_queue_count(&mut self, count: usize) -> Result<(), ChannelError> {
        dispatch_channel!(self, send_queue_count, count)
    }
}
