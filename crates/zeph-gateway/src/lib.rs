// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! HTTP gateway for webhook ingestion with bearer auth and health endpoint.

mod error;
mod handlers;
mod router;
mod server;

pub use error::GatewayError;
pub use server::GatewayServer;
