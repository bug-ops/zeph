// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! stdio transport for the ACP server.
//!
//! The IDE spawns the agent binary and communicates over the process's stdin/stdout
//! pipes using newline-delimited JSON-RPC 2.0 frames (the ACP wire format).
//!
//! Agent session tasks are spawned via `tokio::task::spawn_local` because `Agent<LoopbackChannel>`
//! is `!Send` (async method bodies hold internal references across await points). `serve_stdio`
//! wraps `run_agent` in a `LocalSet` directly. `serve_connection` requires the caller to provide
//! an enclosing `LocalSet`; the HTTP transport satisfies this with a per-connection thread.
//!
//! # SECURITY(layer-2): Session binding limitation
//!
//! stdio transport has no cryptographic session binding. Any process with access
//! to the pipe can inject messages. For multi-tenant scenarios, use the HTTP/WS
//! transport which provides bearer-token session binding.

use std::sync::Arc;

use agent_client_protocol as acp;
use futures::{AsyncWrite, AsyncWriteExt as _};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use zeph_memory::store::SqliteStore;

use crate::agent::{AgentSpawner, ZephAcpAgentState, run_agent};
use crate::error::AcpError;
use crate::transport::{AcpServerConfig, ReadyNotification};

async fn write_ready_notification<W>(
    writer: &mut W,
    ready: &ReadyNotification,
) -> Result<(), AcpError>
where
    W: AsyncWrite + Unpin,
{
    let mut payload = serde_json::Map::new();
    payload.insert(
        "version".into(),
        serde_json::Value::String(ready.version.clone()),
    );
    payload.insert("pid".into(), serde_json::Value::from(ready.pid));
    if let Some(log_file) = &ready.log_file {
        payload.insert(
            "log_file".into(),
            serde_json::Value::String(log_file.clone()),
        );
    }

    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "zeph/ready",
        "params": payload,
    });
    let line = serde_json::to_string(&frame).map_err(|e| AcpError::Transport(e.to_string()))?;
    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|e| AcpError::Transport(e.to_string()))?;
    writer
        .write_all(b"\n")
        .await
        .map_err(|e| AcpError::Transport(e.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|e| AcpError::Transport(e.to_string()))
}

/// Build a [`ZephAcpAgentState`] from the provided configuration.
///
/// Shared by stdio and HTTP transports.
pub(crate) async fn build_agent_state(
    spawner: AgentSpawner,
    server_config: AcpServerConfig,
) -> Arc<ZephAcpAgentState> {
    let mut agent = ZephAcpAgentState::new(
        spawner,
        server_config.max_sessions,
        server_config.session_idle_timeout_secs,
        server_config.permission_file,
    )
    .with_agent_info(server_config.agent_name, server_config.agent_version)
    .with_title_max_chars(server_config.title_max_chars)
    .with_max_history(server_config.max_history);

    if let Some(ref path) = server_config.sqlite_path {
        match SqliteStore::new(path).await {
            Ok(store) => agent = agent.with_store(store),
            Err(e) => tracing::warn!(error = %e, "failed to open ACP SQLite store"),
        }
    }
    if let Some(factory) = server_config.provider_factory {
        agent = agent.with_provider_factory(factory, server_config.available_models);
    }
    if let Some(manager) = server_config.mcp_manager {
        agent = agent.with_mcp_manager(manager);
    }
    if !server_config.project_rules.is_empty() {
        agent = agent.with_project_rules(server_config.project_rules);
    }

    let state = Arc::new(agent);
    state.start_idle_reaper();
    state
}

/// Run the ACP server over stdin/stdout until the connection closes.
///
/// # Errors
///
/// Returns `AcpError::Transport` if the underlying JSON-RPC I/O fails.
pub async fn serve_stdio(
    spawner: AgentSpawner,
    server_config: AcpServerConfig,
) -> Result<(), AcpError> {
    let mut stdout = tokio::io::stdout().compat_write();

    if let Some(ready) = server_config.ready_notification.as_ref() {
        write_ready_notification(&mut stdout, ready).await?;
        tracing::info!(
            transport = "stdio",
            pid = ready.pid,
            version = %ready.version,
            log_file = ready.log_file.as_deref().unwrap_or("<disabled>"),
            "ACP server ready"
        );
    }

    let state = build_agent_state(spawner, server_config).await;

    // Agent session tasks use spawn_local (agent futures are !Send), so the
    // dispatcher loop must run within a LocalSet.
    tokio::task::LocalSet::new()
        .run_until(run_agent(
            state,
            acp::ByteStreams::new(stdout, tokio::io::stdin().compat()),
        ))
        .await
        .map_err(|e| AcpError::Transport(e.to_string()))
}

/// Run the ACP server over arbitrary async byte streams.
///
/// Extracted from [`serve_stdio`] to allow integration tests to use
/// `tokio::io::duplex` or similar in-process transports. The caller must
/// ensure this future runs inside a `tokio::task::LocalSet` (or equivalent)
/// because agent session tasks are spawned via `spawn_local`.
///
/// The HTTP transport satisfies this requirement by running each connection
/// on a dedicated thread with a `current_thread` runtime and `LocalSet`.
///
/// # Errors
///
/// Returns `AcpError::Transport` if the underlying JSON-RPC I/O fails.
pub async fn serve_connection<W, R>(
    spawner: AgentSpawner,
    server_config: AcpServerConfig,
    writer: W,
    reader: R,
) -> Result<(), AcpError>
where
    W: futures::AsyncWrite + Unpin + Send + 'static,
    R: futures::AsyncRead + Unpin + Send + 'static,
{
    let state = build_agent_state(spawner, server_config).await;
    tokio::task::LocalSet::new()
        .run_until(run_agent(state, acp::ByteStreams::new(writer, reader)))
        .await
        .map_err(|e| AcpError::Transport(e.to_string()))
}
