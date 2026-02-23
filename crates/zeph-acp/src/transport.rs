use acp::Client as _;
use agent_client_protocol as acp;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::agent::{AgentSpawner, ZephAcpAgent};
use crate::error::AcpError;

/// Run the ACP server over stdin/stdout until the connection closes.
///
/// Uses `LocalSet` because the ACP SDK's `Agent` trait is `!Send`.
///
/// # Errors
///
/// Returns `AcpError::Transport` if the underlying JSON-RPC I/O fails.
pub async fn serve_stdio(spawner: AgentSpawner) -> Result<(), AcpError> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let stdin = tokio::io::stdin().compat();
            let stdout = tokio::io::stdout().compat_write();

            let (tx, mut rx) = mpsc::unbounded_channel();
            let agent = ZephAcpAgent::new(spawner, tx);

            let (conn, io_fut) = acp::AgentSideConnection::new(agent, stdout, stdin, |fut| {
                tokio::task::spawn_local(fut);
            });

            let mut stream_rx = conn.subscribe();
            let log_messages = std::env::var_os("ZEPH_ACP_LOG_MESSAGES").is_some();
            tokio::task::spawn_local(async move {
                while let Ok(msg) = stream_rx.recv().await {
                    if log_messages {
                        tracing::debug!(
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
