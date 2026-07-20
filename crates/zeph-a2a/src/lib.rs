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
//! `A2aServer` is started as a background service when `[a2a]` is enabled in config. The
//! [`AgentRegistry`] verifies a peer's [`AgentCard`] (signature + URL-origin trust policy,
//! A2A 1.0.0 §8.4) before `zeph --connect <URL>` establishes a session via [`A2aClient`]
//! (#6200); see `src/tui_remote.rs` in the `zeph` binary crate for the wiring.
//!
//! # Features
//!
//! | Feature | Description |
//! |---------|-------------|
//! | `server` | Enables `A2aServer`, `TaskManager`, and `TaskProcessor` |
//! | `ibct`   | Enables [`Ibct`] token issuance and verification (HMAC-SHA256) |
//! | `card-signing` | Enables [`card_signing::verify_card_signatures`] and [`card_signing::sign_card`] (JWS/ES256 over RFC 8785 JCS). Without it, `AgentCardSignature`/`signatures` still (de)serialize, but verification always returns `SignatureVerification::FeatureDisabled`. |
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
//! let client = A2aClient::new_insecure(reqwest::Client::new());
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
pub mod card_signing;
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
///
/// This crate implements A2A **0.2.1** for wire compatibility (method names, well-known
/// discovery path `/.well-known/agent.json`, field shapes), plus one additive 1.0.0
/// feature: [`AgentCard::signatures`](crate::AgentCard::signatures) / [`card_signing`]
/// (A2A 1.0.0 §8.4). This constant is intentionally **not** bumped to `"1.0"` — doing so
/// would over-claim conformance the Key Invariant "`AgentCard` must accurately reflect
/// supported capabilities" forbids. Deferred 1.0.0 items, tracked as follow-ups to #5928:
///
/// - Well-known path rename to `/.well-known/agent-card.json` (see `discovery.rs`).
/// - gRPC / HTTP-REST transport bindings (JSON-RPC only today).
/// - Signing our own served card (`server`/`card.rs` emitting `signatures`).
/// - `jku`/JWKS key retrieval and `x5c` certificate-chain trust anchoring.
pub const A2A_PROTOCOL_VERSION: &str = "0.2.1";

pub use card::AgentCardBuilder;
pub use card_signing::{SigAlg, SignatureVerification, TrustedKey};
pub use client::{A2aClient, SecurityPolicy, TaskEvent, TaskEventStream};
pub use discovery::{AgentRegistry, CardTrustPolicy};
pub use error::A2aError;
pub use ibct::{Ibct, IbctError, IbctKey};
pub use jsonrpc::SendMessageParams;
#[cfg(feature = "server")]
#[cfg_attr(docsrs, doc(cfg(feature = "server")))]
pub use server::{A2aServer, ProcessorEvent, TaskManager, TaskProcessor};
pub use types::*;
