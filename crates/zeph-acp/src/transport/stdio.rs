// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::cell::RefCell;
use std::rc::Rc;

use acp::Client as _;
use agent_client_protocol as acp;
use futures::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::io::duplex;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use zeph_memory::store::SqliteStore;

use crate::agent::{AgentSpawner, ZephAcpAgent};
use crate::error::AcpError;
use crate::transport::{AcpServerConfig, ConnSlot, ReadyNotification};

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

const BRIDGE_BUFFER_SIZE: usize = 64 * 1024;

/// Run the ACP server over stdin/stdout until the connection closes.
///
/// Uses `LocalSet` because the ACP SDK's `Agent` trait is `!Send`.
///
/// # Errors
///
/// Returns `AcpError::Transport` if the underlying JSON-RPC I/O fails.
///
/// # SECURITY(layer-2): Session binding limitation
///
/// stdio transport has no cryptographic session binding. Any process with access to
/// the pipe can inject messages into any session. This is an inherent limitation of
/// stdio IPC. For multi-tenant scenarios, switch to HTTP/WS transport which provides
/// bearer-token session binding.
pub async fn serve_stdio(
    spawner: AgentSpawner,
    server_config: AcpServerConfig,
) -> Result<(), AcpError> {
    let stdin = tokio::io::stdin().compat();
    let stdout = tokio::io::stdout().compat_write();
    serve_connection(spawner, server_config, stdout, stdin).await
}

/// Run the ACP server over arbitrary async I/O streams.
///
/// Extracted from `serve_stdio` to allow integration tests to use `tokio::io::duplex`.
///
/// # Errors
///
/// Returns `AcpError::Transport` if the underlying JSON-RPC I/O fails.
///
/// # Panics
///
/// Panics if the dedicated current-thread Tokio runtime for the ACP control plane
/// cannot be created.
pub async fn serve_connection<W, R>(
    spawner: AgentSpawner,
    server_config: AcpServerConfig,
    mut writer: W,
    mut reader: R,
) -> Result<(), AcpError>
where
    W: AsyncWrite + Unpin + Send + 'static,
    R: AsyncRead + Unpin + Send + 'static,
{
    let (client_w, agent_r) = duplex(BRIDGE_BUFFER_SIZE);
    let (agent_w, client_r) = duplex(BRIDGE_BUFFER_SIZE);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("acp stdio runtime");
        rt.block_on(async move {
            if let Err(e) = serve_connection_local(
                spawner,
                server_config,
                agent_w.compat_write(),
                agent_r.compat(),
            )
            .await
            {
                tracing::error!("ACP stdio connection error: {e}");
            }
        });
    });

    let mut bridge_writer = client_w.compat_write();
    let mut bridge_reader = client_r.compat();
    let inbound = async {
        futures::io::copy(&mut reader, &mut bridge_writer)
            .await
            .map_err(|e| AcpError::Transport(e.to_string()))?;
        bridge_writer
            .close()
            .await
            .map_err(|e| AcpError::Transport(e.to_string()))
    };
    let outbound = async {
        futures::io::copy(&mut bridge_reader, &mut writer)
            .await
            .map_err(|e| AcpError::Transport(e.to_string()))?;
        writer
            .close()
            .await
            .map_err(|e| AcpError::Transport(e.to_string()))
    };

    let _ = futures::future::try_join(inbound, outbound).await?;
    Ok(())
}

pub(crate) async fn serve_connection_local<W, R>(
    spawner: AgentSpawner,
    server_config: AcpServerConfig,
    mut writer: W,
    reader: R,
) -> Result<(), AcpError>
where
    W: AsyncWrite + Unpin + 'static,
    R: AsyncRead + Unpin + 'static,
{
    if let Some(ready) = server_config.ready_notification.as_ref() {
        write_ready_notification(&mut writer, ready).await?;
        tracing::info!(
            transport = "stdio",
            pid = ready.pid,
            version = %ready.version,
            log_file = ready.log_file.as_deref().unwrap_or("<disabled>"),
            "ACP server ready"
        );
    }

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let conn_slot: ConnSlot = Rc::new(RefCell::new(None));

            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut agent = ZephAcpAgent::new(
                spawner,
                tx,
                Rc::clone(&conn_slot),
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
            agent.start_idle_reaper();

            let (conn, io_fut) = acp::AgentSideConnection::new(agent, writer, reader, |fut| {
                tokio::task::spawn_local(fut);
            });

            let conn = Rc::new(conn);
            *conn_slot.borrow_mut() = Some(Rc::clone(&conn));

            let stream_conn = Rc::clone(&conn);
            let log_messages = std::env::var_os("ZEPH_ACP_LOG_MESSAGES").is_some();
            tokio::task::spawn_local(async move {
                let mut stream_rx = stream_conn.subscribe();
                while let Ok(msg) = stream_rx.recv().await {
                    if log_messages {
                        tracing::trace!(
                            direction = ?msg.direction,
                            message = ?msg.message,
                            "ACP stream"
                        );
                    } else {
                        tracing::debug!(direction = ?msg.direction, "ACP stream");
                    }
                }
            });

            tokio::task::spawn_local(async move {
                while let Some((notification, ack)) = rx.recv().await {
                    if let Err(e) = conn.session_notification(notification).await {
                        tracing::error!("session notification error: {e}");
                        break;
                    }
                    ack.send(()).ok();
                }
            });

            io_fut.await.map_err(|e| AcpError::Transport(e.to_string()))
        })
        .await
}
