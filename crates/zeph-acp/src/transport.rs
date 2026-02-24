use std::cell::RefCell;
use std::rc::Rc;

use acp::Client as _;
use agent_client_protocol as acp;
use futures::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::agent::{AgentSpawner, ZephAcpAgent};
use crate::error::AcpError;

/// Shared slot populated after `AgentSideConnection::new` so `new_session` can access
/// the connection to build ACP tool adapters.
pub(crate) type ConnSlot = Rc<RefCell<Option<Rc<acp::AgentSideConnection>>>>;

/// Configuration for the ACP server passed through to the agent.
#[derive(Debug, Clone)]
pub struct AcpServerConfig {
    pub agent_name: String,
    pub agent_version: String,
    pub max_sessions: usize,
    pub session_idle_timeout_secs: u64,
}

impl Default for AcpServerConfig {
    fn default() -> Self {
        Self {
            agent_name: String::new(),
            agent_version: String::new(),
            max_sessions: 4,
            session_idle_timeout_secs: 1800,
        }
    }
}

/// Run the ACP server over stdin/stdout until the connection closes.
///
/// Uses `LocalSet` because the ACP SDK's `Agent` trait is `!Send`.
///
/// # Errors
///
/// Returns `AcpError::Transport` if the underlying JSON-RPC I/O fails.
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
pub async fn serve_connection<W, R>(
    spawner: AgentSpawner,
    server_config: AcpServerConfig,
    writer: W,
    reader: R,
) -> Result<(), AcpError>
where
    W: AsyncWrite + Unpin + 'static,
    R: AsyncRead + Unpin + 'static,
{
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let conn_slot: ConnSlot = Rc::new(RefCell::new(None));

            let (tx, mut rx) = mpsc::unbounded_channel();
            let agent = ZephAcpAgent::new(
                spawner,
                tx,
                Rc::clone(&conn_slot),
                server_config.max_sessions,
                server_config.session_idle_timeout_secs,
            )
            .with_agent_info(server_config.agent_name, server_config.agent_version);
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
                        // Full payload at trace to avoid leaking prompt text and file contents at debug.
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
