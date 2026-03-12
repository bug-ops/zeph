// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "acp-http")]
use tokio::io::DuplexStream;
#[cfg(feature = "acp-http")]
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[cfg(feature = "acp-http")]
use crate::agent::SendAgentSpawner;
#[cfg(feature = "acp-http")]
use crate::transport::{AcpServerConfig, stdio::serve_connection_local};

#[cfg(feature = "acp-http")]
const BRIDGE_BUFFER_SIZE: usize = 64 * 1024;

/// Spawn an ACP connection on a dedicated thread with its own current-thread runtime and `LocalSet`.
///
/// Returns the "external" halves of two duplex channels:
/// - first `DuplexStream`: handler reads agent responses from here
/// - second `DuplexStream`: handler writes client requests here
///
/// The `Agent` trait is `!Send`, so each connection requires its own OS thread.
///
/// # Panics
///
/// Panics if the tokio current-thread runtime for the bridge thread cannot be created.
#[cfg(feature = "acp-http")]
pub fn spawn_acp_connection(
    spawner: SendAgentSpawner,
    server_config: AcpServerConfig,
) -> (DuplexStream, DuplexStream) {
    let (client_w, agent_r) = tokio::io::duplex(BRIDGE_BUFFER_SIZE);
    let (agent_w, client_r) = tokio::io::duplex(BRIDGE_BUFFER_SIZE);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("acp bridge runtime");
        rt.block_on(async {
            let writer = agent_w.compat_write();
            let reader = agent_r.compat();
            if let Err(e) = serve_connection_local(spawner, server_config, writer, reader).await {
                tracing::error!("ACP bridge connection error: {e}");
            }
        });
    });

    (client_r, client_w)
}
