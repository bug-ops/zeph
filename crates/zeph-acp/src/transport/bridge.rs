// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bridge between the HTTP/WebSocket transports and the ACP agent.
//!
//! Each HTTP/WebSocket connection needs its own task running the ACP agent loop.
//! [`spawn_acp_connection`] creates in-process duplex channels and spawns
//! a `tokio::spawn` task running the agent on one end.

#[cfg(feature = "acp-http")]
use tokio::io::DuplexStream;
#[cfg(feature = "acp-http")]
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[cfg(feature = "acp-http")]
use crate::agent::SendAgentSpawner;
#[cfg(feature = "acp-http")]
use crate::transport::AcpServerConfig;

#[cfg(feature = "acp-http")]
const BRIDGE_BUFFER_SIZE: usize = 64 * 1024;

/// Spawn an ACP connection for a single HTTP/WebSocket client.
///
/// Returns two [`DuplexStream`]s:
/// - first: caller reads agent responses from here
/// - second: caller writes client requests here
///
/// # Panics
///
/// Panics if the tokio runtime is not available (should never happen in normal use).
#[cfg(feature = "acp-http")]
pub fn spawn_acp_connection(
    spawner: SendAgentSpawner,
    server_config: AcpServerConfig,
) -> (DuplexStream, DuplexStream) {
    let (client_w, agent_r) = tokio::io::duplex(BRIDGE_BUFFER_SIZE);
    let (agent_w, client_r) = tokio::io::duplex(BRIDGE_BUFFER_SIZE);
    tokio::spawn(async move {
        let writer = agent_w.compat_write();
        let reader = agent_r.compat();
        if let Err(e) =
            crate::transport::stdio::serve_connection(spawner, server_config, writer, reader).await
        {
            tracing::error!("ACP bridge connection error: {e}");
        }
    });
    (client_r, client_w)
}
