// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A2A (Agent-to-Agent) protocol client, server, and agent discovery for Zeph.
//!
//! This crate implements the [A2A protocol](https://google.github.io/A2A/) — a JSON-RPC 2.0
//! based specification for communication between AI agents. It provides:
//!
//! - **Client** ([`A2aClient`]): sends messages and streams responses to remote A2A agents.
//! - **Server** (`A2aServer`, feature `server`): exposes an HTTP endpoint that accepts
//!   A2A JSON-RPC requests and streams Server-Sent Events (SSE) for real-time output.
//! - **Discovery** ([`AgentRegistry`]): fetches and caches agent capability cards from
//!   `/.well-known/agent.json` with configurable TTL.
//! - **Capability cards** ([`AgentCardBuilder`]): builds [`AgentCard`] metadata describing
//!   the agent's skills, I/O modes, and protocol version.
//! - **IBCT** ([`Ibct`], feature `ibct`): Invocation-Bound Capability Tokens for scoped
//!   delegation — HMAC-SHA256 signed tokens bound to a specific task and endpoint.
//! - **JSON-RPC 2.0 types** ([`jsonrpc`]): request/response envelope types and the A2A
//!   method name constants.
//! - **Protocol types** ([`types`]): shared wire-format types re-exported at the crate root.
//!
//! # Architecture
//!
//! `zeph-a2a` is an optional feature-gated dependency of the main `zeph` binary. The
//! `A2aServer` is started by `zeph-core` as a background service when `[a2a]` is enabled
//! in config. The [`A2aClient`] is used by the agent to delegate tasks to peer agents
//! discovered through the [`AgentRegistry`].
//!
//! # Features
//!
//! | Feature | Description |
//! |---------|-------------|
//! | `server` | Enables `A2aServer`, `TaskManager`, and `TaskProcessor` |
//! | `ibct`   | Enables [`Ibct`] token issuance and verification (HMAC-SHA256) |
//!
//! # Examples
//!
//! ```rust,no_run
//! use zeph_a2a::{A2aClient, AgentCardBuilder, AgentRegistry, SendMessageParams, Message};
//! use std::time::Duration;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Build an agent card for this agent.
//! let card = AgentCardBuilder::new("my-agent", "http://localhost:8080", "0.1.0")
//!     .description("A helpful AI agent")
//!     .streaming(true)
//!     .build();
//!
//! // Discover a peer agent's capabilities.
//! let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_secs(300));
//! let peer_card = registry.discover("http://peer-agent.example.com").await?;
//!
//! // Send a message to the peer agent.
//! let client = A2aClient::new(reqwest::Client::new());
//! let params = SendMessageParams {
//!     message: Message::user_text("Hello, peer agent!"),
//!     configuration: None,
//! };
//! let task = client.send_message(&peer_card.url, params, None).await?;
//! println!("Task {} in state {:?}", task.id, task.status.state);
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod card;
pub mod client;
pub mod discovery;
pub mod error;
pub mod ibct;
pub mod jsonrpc;
#[cfg(feature = "server")]
#[cfg_attr(docsrs, doc(cfg(feature = "server")))]
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
pub use ibct::{Ibct, IbctError, IbctKey};
pub use jsonrpc::SendMessageParams;
#[cfg(feature = "server")]
#[cfg_attr(docsrs, doc(cfg(feature = "server")))]
pub use server::{A2aServer, ProcessorEvent, TaskManager, TaskProcessor};
pub use types::*;
