//! Error types for zeph-channels.

/// Errors that can occur in channel operations.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    /// Telegram API error.
    #[error("Telegram API error: {0}")]
    Telegram(String),

    /// No active chat available for operation.
    #[error("no active chat")]
    NoActiveChat,
}
