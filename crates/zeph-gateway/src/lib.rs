// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! HTTP gateway for webhook ingestion with bearer-token auth and health endpoint.
//!
//! `zeph-gateway` exposes two HTTP endpoints over a single TCP listener:
//!
//! | Endpoint | Method | Auth required | Purpose |
//! |---|---|---|---|
//! | `/health` | GET | No | Liveness check; returns uptime in seconds |
//! | `/webhook` | POST | Yes (if token set) | Ingest external events into the agent |
//!
//! # Security model
//!
//! - Bearer token comparison is performed in constant time via BLAKE3 + `subtle::ConstantTimeEq`
//!   to prevent timing-oracle attacks (see [`GatewayServer::with_auth`]).
//! - A bearer token is mandatory: [`GatewayServer::serve`] refuses to start
//!   ([`GatewayError::MissingAuthToken`]) when no non-empty token is configured, since
//!   `/webhook` forwards its body directly into the agent's turn loop (#6487).
//! - Payload fields are sanitised with [`zeph_common::sanitize`] before forwarding.
//!
//! # Rate limiting
//!
//! Requests to `/webhook` are rate-limited per remote IP using a fixed-window counter with a
//! 60-second window. The default ceiling is 120 requests per window and is configurable via
//! [`GatewayServer::with_rate_limit`]. Setting the limit to `0` disables rate limiting.
//!
//! # Quick start
//!
//! ```no_run
//! use tokio::sync::{mpsc, watch};
//! use zeph_gateway::{GatewayServer, WebhookMessage};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let (webhook_tx, mut webhook_rx) = mpsc::channel::<WebhookMessage>(64);
//!     let (_shutdown_tx, shutdown_rx) = watch::channel(false);
//!
//!     tokio::spawn(async move { // EXEMPT: doc-example only
//!         while let Some(msg) = webhook_rx.recv().await {
//!             println!("received: {msg:?}");
//!         }
//!     });
//!
//!     GatewayServer::new("127.0.0.1", 8080, webhook_tx, shutdown_rx)
//!         .with_auth(Some("my-secret-token".into()))
//!         .with_rate_limit(60)
//!         .serve()
//!         .await?;
//!
//!     Ok(())
//! }
//! ```

mod error;
mod handlers;
mod router;
mod server;

pub use error::GatewayError;
pub use handlers::WebhookMessage;
pub use server::GatewayServer;
