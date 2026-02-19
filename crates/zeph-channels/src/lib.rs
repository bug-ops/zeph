//! Channel implementations for the Zeph agent.

mod any;
pub mod cli;
#[cfg(feature = "discord")]
pub mod discord;
pub mod error;
mod line_editor;
pub mod markdown;
#[cfg(feature = "slack")]
pub mod slack;
pub mod telegram;

pub use any::AnyChannel;
pub use cli::CliChannel;
pub use error::ChannelError;
