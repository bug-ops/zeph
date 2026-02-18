//! Agent loop, configuration loading, and context builder.

pub mod agent;
#[allow(clippy::missing_errors_doc, clippy::must_use_candidate)]
pub mod bootstrap;
pub mod channel;
pub mod config;
pub mod config_watcher;
pub mod context;
pub mod cost;
#[cfg(feature = "daemon")]
pub mod daemon;
pub mod metrics;
pub mod pipeline;
pub mod project;
pub mod redact;
pub mod vault;

pub mod diff;

pub use agent::Agent;
pub use agent::error::AgentError;
pub use channel::{Attachment, AttachmentKind, Channel, ChannelError, ChannelMessage};
pub use config::Config;
pub use diff::DiffData;
