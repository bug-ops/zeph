//! Channel implementations for the Zeph agent.

pub mod cli;
#[cfg(feature = "discord")]
pub mod discord;
pub mod error;
pub mod markdown;
#[cfg(feature = "slack")]
pub mod slack;
pub mod telegram;

pub use cli::CliChannel;
pub use error::ChannelError;
