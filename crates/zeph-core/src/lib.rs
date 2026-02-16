//! Agent loop, configuration loading, and context builder.

pub mod agent;
pub mod channel;
pub mod config;
pub mod config_watcher;
pub mod context;
pub mod cost;
pub mod metrics;
pub mod project;
pub mod redact;
pub mod vault;

pub use agent::Agent;
pub use agent::error::AgentError;
pub use channel::{Channel, ChannelError, ChannelMessage};
pub use config::Config;
