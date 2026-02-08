#![forbid(unsafe_code)]

pub mod card;
pub mod client;
pub mod discovery;
pub mod error;
pub mod jsonrpc;
pub mod types;

pub use card::AgentCardBuilder;
pub use client::{A2aClient, TaskEvent, TaskEventStream};
pub use discovery::AgentRegistry;
pub use error::A2aError;
pub use types::*;
