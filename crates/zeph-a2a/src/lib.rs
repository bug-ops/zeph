// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#![forbid(unsafe_code)]

pub mod card;
pub mod client;
pub mod discovery;
pub mod error;
pub mod jsonrpc;
#[cfg(feature = "server")]
pub mod server;
pub mod types;

#[cfg(test)]
mod testing;

/// A2A protocol version implemented by this crate.
pub const A2A_PROTOCOL_VERSION: &str = "0.2.1";

pub use card::AgentCardBuilder;
pub use client::{A2aClient, TaskEvent, TaskEventStream};
pub use discovery::AgentRegistry;
pub use error::A2aError;
pub use jsonrpc::SendMessageParams;
#[cfg(feature = "server")]
pub use server::{A2aServer, ProcessorEvent, TaskManager, TaskProcessor};
pub use types::*;
